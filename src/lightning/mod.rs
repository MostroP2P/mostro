pub mod invoice;

use crate::config::settings::Settings;
use crate::lightning::invoice::decode_invoice;
use crate::util::bytes_to_string;
use bitcoin::hashes::hex::FromHex;
use easy_hasher::easy_hasher::*;
use fedimint_tonic_lnd::invoicesrpc::{
    AddHoldInvoiceRequest, AddHoldInvoiceResp, CancelInvoiceMsg, CancelInvoiceResp,
    SettleInvoiceMsg, SettleInvoiceResp,
};
use fedimint_tonic_lnd::lnrpc::{
    invoice::InvoiceState, payment, GetInfoRequest, GetInfoResponse, InvoiceHtlcState, Payment,
    PaymentHash,
};
use fedimint_tonic_lnd::routerrpc::{SendPaymentRequest, TrackPaymentRequest};
use fedimint_tonic_lnd::Client;
use mostro_core::prelude::*;
use rand::{self, RngCore};
use std::cmp::Ordering;
use std::time::Duration;
use tokio::sync::mpsc::Sender;
use tokio::time::timeout;
use tracing::info;

/// Seconds LND keeps launching route attempts for a payment
/// (`SendPaymentRequest.timeout_seconds`). Past this window an open payment
/// stream is only kept alive by an HTLC that is locked-in but unresolved,
/// which the sender cannot cancel.
pub(crate) const LND_PAYMENT_ROUTE_TIMEOUT_SECS: i32 = 60;

/// Upper bound on how long a payout waits for `send_payment` to reach a
/// terminal state: LND's own route-attempt window plus margin, DERIVED so
/// that raising [`LND_PAYMENT_ROUTE_TIMEOUT_SECS`] can never silently
/// undercut LND's retries. Hitting this bound does NOT fail the payout —
/// the payment may still settle, so callers keep their claim/hash and let
/// reconciliation resolve the real outcome. Fixed for now; could become a
/// settings knob later.
pub(crate) const PAYOUT_SEND_PAYMENT_TIMEOUT: Duration =
    Duration::from_secs(LND_PAYMENT_ROUTE_TIMEOUT_SECS as u64 + 15);

/// Bound on the duplicate-guard lookup inside `send_payment`. The guard is
/// advisory — on timeout or transport error the send proceeds, because LND
/// itself rejects a genuine duplicate for an in-flight/settled hash — so it
/// must never eat a caller's whole budget: `dev_fee::send_dev_fee_payment`
/// wraps `send_payment` in a 5s total timeout, and 2s leaves the majority of
/// that for the send itself.
const DUPLICATE_GUARD_LOOKUP_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone)]
pub struct LndConnector {
    pub client: Client,
}

#[derive(Debug, Clone)]
pub struct InvoiceMessage {
    pub hash: Vec<u8>,
    pub state: InvoiceState,
}

#[derive(Debug, Clone)]
pub struct PaymentMessage {
    pub payment: Payment,
}

/// Blocks a route needs *on top of* the payout invoice's own final CLTV
/// delta before [`payment_cltv_limit_blocks`] will hand an operator's ceiling
/// to LND. Six hops at 96 blocks each — comfortably above LND's default
/// forwarding delta and above what CLN and Eclair ask for — so an operator
/// cannot quietly configure a ceiling that fails every honest payout while
/// looking like a tighter security setting.
pub(crate) const MIN_ROUTE_CLTV_HEADROOM: u32 = 576;

/// Total-timelock ceiling handed to LND as `SendPaymentRequest.cltv_limit`,
/// or `0` to let the node apply its own `--max-cltv-expiry`.
///
/// `max_final_cltv_expiry_delta` bounds only the payee's own hop: how long an
/// unsettling payee can sit on the HTLC. This bounds the whole route, which
/// is what the node's channel is really locked for if a hop force-closes and
/// the HTLC has to resolve on-chain.
///
/// Two ways a configured ceiling is unusable, and both defer to the node
/// rather than substituting a number of our own:
///
/// - `0` is the operator asking for exactly that, and it is the escape hatch
///   for a node whose `--max-cltv-expiry` sits below what Mostro would send.
/// - Below `final_delta_bound + MIN_ROUTE_CLTV_HEADROOM` there is no room for
///   a route, so pathfinding would reject paths honest wallets produce and
///   every payout would fail with "no route" — looking like a routing problem
///   rather than a misconfiguration.
///
/// The second case is logged at error level and then deferred, deliberately
/// *not* raised to the floor: LND rejects a `cltv_limit` above its own
/// `--max-cltv-expiry` outright, so inventing a ceiling to replace a bad one
/// risks trading "no route" for "every payout rejected". Deferring restores
/// the node's own bound, which is what applied before this setting existed.
pub fn payment_cltv_limit_blocks(configured: u32, final_delta_bound: u32) -> i32 {
    if configured == 0 {
        return 0;
    }

    let floor = final_delta_bound.saturating_add(MIN_ROUTE_CLTV_HEADROOM);
    if configured < floor {
        tracing::error!(
            "payment_cltv_limit ({configured}) leaves no room for a route above \
             max_final_cltv_expiry_delta ({final_delta_bound}); it needs to be at \
             least {floor}. Deferring to the node's --max-cltv-expiry until it is \
             fixed — payouts are unbounded by this setting in the meantime"
        );
        return 0;
    }

    // `cltv_limit` is an i32 on the wire. Saturating keeps an absurd
    // configuration from wrapping into a negative — which LND would read as a
    // ceiling far tighter than intended.
    i32::try_from(configured).unwrap_or(i32::MAX)
}

/// Whether LND turned a `SendPayment` down over the `cltv_limit` we asked for.
///
/// LND refuses a `cltv_limit` above its own `--max-cltv-expiry` (default 2016)
/// before dispatching anything, so an operator who lowered that setting below
/// `payment_cltv_limit` would otherwise see *every* payout — buyer, bond and
/// dev fee — rejected. Matching the message lets the caller retry once without
/// the ceiling instead, which is the behaviour that applied before the setting
/// existed.
fn is_cltv_limit_rejection(message: &str) -> bool {
    message.to_lowercase().contains("cltv")
}

/// Routing-fee cap (in sats) handed to LND as `fee_limit_sat` for a
/// payment of `amount` sats.
///
/// This is the single source of truth for the cap. Both the actual
/// payment (`LndConnector::send_payment`) and the value persisted for
/// operator debugging (`bonds.payout_routing_fee_sats`) derive from it,
/// so the recorded number always matches what LND enforced.
pub fn routing_fee_cap_sats(amount: i64) -> i64 {
    let max_routing_fee = Settings::get_mostro().max_routing_fee;
    // If the amount is small we use a different max routing fee.
    let max_fee = match amount.cmp(&1000) {
        Ordering::Less | Ordering::Equal => {
            // For small amounts, use 1% but ensure minimum of 10 sats
            // to allow routing (otherwise tiny amounts like 30 sats would have 0 fee limit)
            (amount as f64 * 0.01).max(10.0)
        }
        Ordering::Greater => amount as f64 * max_routing_fee,
    };
    max_fee as i64
}

/// Length in bytes of a Lightning payment preimage and of the payment
/// hash derived from it (both are SHA-256 sized).
const HASH_LEN: usize = 32;

/// Decode a hex-encoded 32-byte preimage or payment hash — as stored in
/// the `orders` / `bonds` tables — into the raw bytes LND expects.
///
/// This must never panic. The main event loop in `src/app.rs` processes
/// messages sequentially on a single task with no panic boundary, so an
/// `.expect()` here would turn a single malformed row (corruption, a
/// partial write, a manual DB edit) into a full-daemon outage for every
/// user of the instance. Returning a typed error keeps the blast radius
/// at the one operation that touched the bad row.
///
/// `field` names the column for the log line. The value itself is never
/// included in the error: the preimage is the secret that claims the
/// HTLC, and errors end up in logs.
pub(crate) fn decode_hash32(field: &str, value: &str) -> Result<Vec<u8>, MostroError> {
    let bytes = Vec::<u8>::from_hex(value).map_err(|e| {
        MostroInternalErr(ServiceError::HoldInvoiceError(format!(
            "invalid {field}: not valid hex ({e})"
        )))
    })?;

    if bytes.len() != HASH_LEN {
        return Err(MostroInternalErr(ServiceError::HoldInvoiceError(format!(
            "invalid {field}: expected {} bytes, got {}",
            HASH_LEN,
            bytes.len()
        ))));
    }

    Ok(bytes)
}

impl LndConnector {
    pub async fn new() -> Result<Self, MostroError> {
        let ln_settings = Settings::get_ln();

        // Connecting to LND requires only host, port, cert file, and macaroon file
        let client = fedimint_tonic_lnd::connect(
            ln_settings.lnd_grpc_host.clone(),
            ln_settings.lnd_cert_file.clone(),
            ln_settings.lnd_macaroon_file.clone(),
        )
        .await
        .map_err(|e| MostroInternalErr(ServiceError::LnNodeError(e.to_string())))?;

        // Safe unwrap here
        Ok(Self { client })
    }

    pub async fn create_hold_invoice(
        &mut self,
        description: &str,
        amount: i64,
    ) -> Result<(AddHoldInvoiceResp, Vec<u8>, Vec<u8>), MostroError> {
        let mut preimage = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut preimage);
        let hash = raw_sha256(preimage.to_vec());
        let ln_settings = Settings::get_ln();
        let cltv_expiry = ln_settings.hold_invoice_cltv_delta as u64;

        let invoice = AddHoldInvoiceRequest {
            hash: hash.to_vec(),
            memo: description.to_string(),
            value: amount,
            cltv_expiry,
            ..Default::default()
        };
        let holdinvoice = self
            .client
            .invoices()
            .add_hold_invoice(invoice)
            .await
            .map_err(|e| MostroInternalErr(ServiceError::LnNodeError(e.to_string())));

        match holdinvoice {
            Ok(holdinvoice) => Ok((holdinvoice.into_inner(), preimage.to_vec(), hash.to_vec())),
            Err(e) => Err(MostroInternalErr(ServiceError::LnNodeError(e.to_string()))),
        }
    }

    pub async fn subscribe_invoice(
        &mut self,
        r_hash: Vec<u8>,
        listener: Sender<InvoiceMessage>,
    ) -> Result<(), MostroError> {
        let invoice_stream = self
            .client
            .invoices()
            .subscribe_single_invoice(
                fedimint_tonic_lnd::invoicesrpc::SubscribeSingleInvoiceRequest {
                    r_hash: r_hash.clone(),
                },
            )
            .await
            .map_err(|e| MostroInternalErr(ServiceError::LnNodeError(e.to_string())))?;

        let mut inner_invoice = invoice_stream.into_inner();

        while let Some(invoice) = inner_invoice
            .message()
            .await
            .map_err(|e| MostroInternalErr(ServiceError::LnNodeError(e.to_string())))?
        {
            let state = fedimint_tonic_lnd::lnrpc::invoice::InvoiceState::try_from(invoice.state)
                .map_err(|e| MostroInternalErr(ServiceError::LnNodeError(e.to_string())))?;
            {
                let msg = InvoiceMessage {
                    hash: r_hash.clone(),
                    state,
                };
                listener
                    .clone()
                    .send(msg)
                    .await
                    .map_err(|e| MostroInternalErr(ServiceError::LnNodeError(e.to_string())))?
            }
        }
        Ok(())
    }

    pub async fn settle_hold_invoice(
        &mut self,
        preimage: &str,
    ) -> Result<SettleInvoiceResp, MostroError> {
        let preimage = decode_hash32("preimage", preimage)?;

        let preimage_message = SettleInvoiceMsg { preimage };
        let settle = self
            .client
            .invoices()
            .settle_invoice(preimage_message)
            .await
            .map_err(|e| MostroInternalErr(ServiceError::LnNodeError(e.to_string())));

        match settle {
            Ok(settle) => Ok(settle.into_inner()),
            Err(e) => Err(e),
        }
    }

    pub async fn cancel_hold_invoice(
        &mut self,
        hash: &str,
    ) -> Result<CancelInvoiceResp, MostroError> {
        let payment_hash = decode_hash32("payment hash", hash)?;

        let cancel_message = CancelInvoiceMsg { payment_hash };
        let cancel = self.client.invoices().cancel_invoice(cancel_message).await;

        match cancel {
            Ok(cancel) => Ok(cancel.into_inner()),
            Err(status) => {
                // Preserve the gRPC code in the error string with a stable
                // `code=<Code>` prefix. Bond release uses this to tell
                // benign "already canceled / not found" outcomes from
                // transient transport failures so it can avoid marking a
                // bond Released while the HTLC may still be encumbered.
                Err(MostroInternalErr(ServiceError::LnNodeError(format!(
                    "code={:?} message={}",
                    status.code(),
                    status.message()
                ))))
            }
        }
    }

    /// Current chain tip height as seen by LND, for CLTV-deadline math.
    pub async fn get_chain_height(&mut self) -> Result<u32, MostroError> {
        self.get_node_info().await.map(|info| info.block_height)
    }

    /// Earliest CLTV expiry height among the ACCEPTED HTLCs backing the
    /// hold invoice `hash` (hex). `None` when no accepted HTLC backs it
    /// anymore — the invoice was canceled, settled, or never held.
    pub async fn get_hold_invoice_expiry_height(
        &mut self,
        hash: &str,
    ) -> Result<Option<u32>, MostroError> {
        let r_hash = decode_hash32("payment hash", hash)?;

        let invoice = self
            .client
            .lightning()
            .lookup_invoice(PaymentHash {
                r_hash,
                ..Default::default()
            })
            .await
            .map_err(|e| MostroInternalErr(ServiceError::LnNodeError(e.to_string())))?
            .into_inner();

        Ok(invoice
            .htlcs
            .iter()
            .filter(|htlc| htlc.state == InvoiceHtlcState::Accepted as i32)
            .map(|htlc| htlc.expiry_height.max(0) as u32)
            .min())
    }

    pub async fn send_payment(
        &mut self,
        payment_request: &str,
        amount: i64,
        listener: Sender<PaymentMessage>,
    ) -> Result<(), MostroError> {
        let invoice = decode_invoice(payment_request)?;
        // The BOLT11 payment hash — the key LND indexes payments by. NOT
        // `signable_hash()`, which is the invoice's signature digest and is
        // never known to LND, so a guard keyed on it can never fire.
        let payment_hash_ref: &[u8] = invoice.payment_hash().as_ref();
        let payment_hash = payment_hash_ref.to_vec();
        let hash = bytes_to_string(&payment_hash);

        // We need to set a max fee amount. `routing_fee_cap_sats` is the
        // single source of truth so the value persisted for operator
        // debugging always matches what LND actually enforces.
        let max_fee = routing_fee_cap_sats(amount);

        // Duplicate-dispatch guard: refuse to send only when LND reports this
        // hash as already in flight or settled. A Failed/Unknown/absent record
        // must NOT abort — the retry flow legitimately re-sends the same
        // invoice after a failure. A lookup transport error or timeout also
        // proceeds: LND itself rejects a duplicate SendPaymentV2 for an
        // in-flight or settled hash (the hard backstop behind this check),
        // and if LND is truly unreachable the send below fails anyway. The
        // lookup is bounded so this advisory check can never eat a caller's
        // budget (see DUPLICATE_GUARD_LOOKUP_TIMEOUT).
        match timeout(
            DUPLICATE_GUARD_LOOKUP_TIMEOUT,
            self.lookup_payment_status(&payment_hash),
        )
        .await
        {
            Ok(Ok(Some(payment::PaymentStatus::InFlight)))
            | Ok(Ok(Some(payment::PaymentStatus::Succeeded))) => {
                info!(
                    "Aborting payment for hash {}: already in flight or settled",
                    hash
                );
                return Err(MostroInternalErr(ServiceError::LnPaymentError(
                    "payment already dispatched for this hash".to_string(),
                )));
            }
            // The degraded paths must not be silent: an LND whose
            // track_payment_v2 consistently exceeds the bound would leave
            // this guard permanently disabled with no trace in the logs.
            Err(_) => info!(
                "Duplicate guard lookup for hash {} timed out after {}s; proceeding (LND rejects real duplicates)",
                hash,
                DUPLICATE_GUARD_LOOKUP_TIMEOUT.as_secs()
            ),
            Ok(Err(e)) => info!(
                "Duplicate guard lookup for hash {} failed ({e}); proceeding",
                hash
            ),
            // Failed / Unknown / no record: the normal go-ahead.
            _ => {}
        }

        // Bound the total route timelock too, not just the payee's own hop.
        // Left at the proto default of 0, LND would apply its own
        // `--max-cltv-expiry` (2016 blocks) and the operator's setting would
        // have no say in how long a channel can stay locked.
        let ln_settings = Settings::get_ln();
        let cltv_limit = payment_cltv_limit_blocks(
            ln_settings.payment_cltv_limit,
            ln_settings.max_final_cltv_expiry_delta,
        );

        let mut request = SendPaymentRequest {
            payment_request: payment_request.to_string(),
            timeout_seconds: LND_PAYMENT_ROUTE_TIMEOUT_SECS,
            fee_limit_sat: max_fee,
            cltv_limit,
            ..Default::default()
        };
        let invoice_amount_milli = invoice.amount_milli_satoshis();
        match invoice_amount_milli {
            Some(amt) => {
                if amt != amount as u64 * 1000 {
                    info!(
                        "Aborting paying invoice with wrong amount to buyer, hash: {}",
                        hash
                    );
                    return Err(MostroInternalErr(ServiceError::LnPaymentError(
                        "Wrong amount".to_string(),
                    )));
                }
            }
            None => {
                // We add amount to the request only if the invoice doesn't have amount
                request = SendPaymentRequest {
                    amt: amount,
                    ..request
                };
            }
        }

        let mut outer_stream = self.client.router().send_payment_v2(request.clone()).await;

        // An operator whose node has a `--max-cltv-expiry` below our ceiling
        // gets this request turned down before anything is dispatched, which
        // would mean *every* payout failing after an upgrade. Retry once
        // without the ceiling — the node's own bound then applies, exactly as
        // it did before this setting existed — and make the misconfiguration
        // loud. The retry cannot double-pay: LND refuses a second
        // `SendPaymentV2` for a hash already in flight or settled, and this
        // first call never got far enough to create one.
        if cltv_limit != 0 {
            if let Err(status) = &outer_stream {
                if is_cltv_limit_rejection(status.message()) {
                    tracing::error!(
                        "LND refused payment_cltv_limit ({cltv_limit}) for hash {}: {}. \
                         Retrying without it — set payment_cltv_limit at or below the \
                         node's --max-cltv-expiry, or to 0 to defer to it",
                        hash,
                        status.message()
                    );
                    request.cltv_limit = 0;
                    outer_stream = self.client.router().send_payment_v2(request).await;
                }
            }
        }

        let mut stream = outer_stream
            .map_err(|e| MostroInternalErr(ServiceError::LnPaymentError(e.to_string())))?
            .into_inner();

        while let Ok(Some(payment)) = stream
            .message()
            .await
            .map_err(|e| MostroInternalErr(ServiceError::LnPaymentError(e.to_string())))
        {
            //   ("Failed paying invoice") {
            let msg = PaymentMessage { payment };
            listener
                .clone()
                .send(msg)
                .await
                .map_err(|e| MostroInternalErr(ServiceError::LnNodeError(e.to_string())))?
        }

        Ok(())
    }

    /// Look up a payment by hash, distinguishing "LND has no record" from
    /// transport errors.
    ///
    /// Used by the bond payout flow to reconcile after a successful
    /// `send_payment` whose follow-up DB write failed: on the next
    /// scheduler tick `pay_counterparty` queries LND for the persisted
    /// `payout_payment_hash` and only re-invokes `send_payment` if LND
    /// confirms it never saw the hash.
    ///
    /// Returns:
    /// - `Ok(Some(status))` — LND tracks this hash and reports `status`.
    /// - `Ok(None)` — LND has no record of this hash (`NotFound`). The
    ///   hash may never have been attempted, or LND pruned the record.
    /// - `Err(_)` — transport / gRPC error; status is unknown.
    pub async fn lookup_payment_status(
        &mut self,
        payment_hash: &[u8],
    ) -> Result<Option<fedimint_tonic_lnd::lnrpc::payment::PaymentStatus>, MostroError> {
        let track_req = TrackPaymentRequest {
            payment_hash: payment_hash.to_vec(),
            no_inflight_updates: false,
        };

        let stream = match self.client.router().track_payment_v2(track_req).await {
            Ok(s) => s,
            Err(status) => {
                if status.code() == fedimint_tonic_lnd::tonic::Code::NotFound {
                    return Ok(None);
                }
                return Err(MostroInternalErr(ServiceError::LnPaymentError(format!(
                    "code={:?} message={}",
                    status.code(),
                    status.message()
                ))));
            }
        };

        let mut stream = stream.into_inner();
        match stream.message().await {
            Ok(Some(payment)) => {
                let status =
                    fedimint_tonic_lnd::lnrpc::payment::PaymentStatus::try_from(payment.status)
                        .map_err(|_| {
                            MostroInternalErr(ServiceError::LnPaymentError(
                                "Unknown payment status".to_string(),
                            ))
                        })?;
                Ok(Some(status))
            }
            Ok(None) => Ok(None),
            Err(status) => {
                if status.code() == fedimint_tonic_lnd::tonic::Code::NotFound {
                    Ok(None)
                } else {
                    Err(MostroInternalErr(ServiceError::LnPaymentError(format!(
                        "code={:?} message={}",
                        status.code(),
                        status.message()
                    ))))
                }
            }
        }
    }

    /// Query the current status of a payment by its hash.
    ///
    /// Returns the LND `PaymentStatus` if the payment is found, or an error
    /// if the payment cannot be tracked (e.g., unknown hash).
    pub async fn check_payment_status(
        &mut self,
        payment_hash: &[u8],
    ) -> Result<fedimint_tonic_lnd::lnrpc::payment::PaymentStatus, MostroError> {
        let track_req = TrackPaymentRequest {
            payment_hash: payment_hash.to_vec(),
            no_inflight_updates: false,
        };

        let mut stream = self
            .client
            .router()
            .track_payment_v2(track_req)
            .await
            .map_err(|e| MostroInternalErr(ServiceError::LnPaymentError(e.to_string())))?
            .into_inner();

        // Get the first (current) status update
        match stream.message().await {
            Ok(Some(payment)) => fedimint_tonic_lnd::lnrpc::payment::PaymentStatus::try_from(
                payment.status,
            )
            .map_err(|_| {
                MostroInternalErr(ServiceError::LnPaymentError(
                    "Unknown payment status".to_string(),
                ))
            }),
            Ok(None) => Err(MostroInternalErr(ServiceError::LnPaymentError(
                "No payment status received (stream ended)".to_string(),
            ))),
            Err(e) => Err(MostroInternalErr(ServiceError::LnPaymentError(format!(
                "Failed to get payment status: {}",
                e
            )))),
        }
    }

    pub async fn get_node_info(&mut self) -> Result<GetInfoResponse, MostroError> {
        let info = self.client.lightning().get_info(GetInfoRequest {}).await;

        match info {
            Ok(i) => Ok(i.into_inner()),
            Err(e) => Err(MostroInternalErr(ServiceError::LnNodeError(e.to_string()))),
        }
    }
}

#[derive(Debug)]
pub struct LnStatus {
    pub version: String,
    pub node_pubkey: String,
    pub commit_hash: String,
    pub node_alias: String,
    pub chains: Vec<String>,
    pub networks: Vec<String>,
    pub uris: Vec<String>,
}

impl LnStatus {
    pub fn from_get_info_response(info: GetInfoResponse) -> Self {
        Self {
            version: info.version,
            node_pubkey: info.identity_pubkey,
            commit_hash: info.commit_hash,
            node_alias: info.alias,
            chains: info.chains.iter().map(|c| c.chain.to_string()).collect(),
            networks: info.chains.iter().map(|c| c.network.to_string()).collect(),
            uris: info.uris.iter().map(|u| u.to_string()).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        decode_hash32, is_cltv_limit_rejection, payment_cltv_limit_blocks, routing_fee_cap_sats,
        MIN_ROUTE_CLTV_HEADROOM,
    };
    use crate::config::settings::Settings;
    use crate::config::MOSTRO_CONFIG;
    use mostro_core::prelude::*;

    fn init_test_settings() {
        crate::config::init_test_nostr_keys();
        // Defaults set `max_routing_fee = 0.002`.
        let _ = MOSTRO_CONFIG.set(Settings {
            database: Default::default(),
            nostr: crate::config::NostrSettings {
                nsec_privkey: secrecy::SecretString::from(
                    "nsec13as48eum93hkg7plv526r9gjpa0uc52zysqm93pmnkca9e69x6tsdjmdxd",
                ),
                relays: vec![],
            },
            mostro: Default::default(),
            lightning: Default::default(),
            rpc: Default::default(),
            expiration: Some(Default::default()),
            anti_abuse_bond: None,
            cashu: None,
            price: None,
        });
    }

    /// A ceiling with room for a route is passed through untouched: this is
    /// the whole point of the setting, and silently rewriting an operator's
    /// value would make the knob meaningless.
    #[test]
    fn payment_cltv_limit_honours_a_ceiling_that_leaves_room_for_a_route() {
        assert_eq!(payment_cltv_limit_blocks(1008, 432), 1008);
        assert_eq!(payment_cltv_limit_blocks(2016, 144), 2016);
    }

    /// The shipped defaults must clear the floor on their own, or every node
    /// would boot straight into the defer path and run with no ceiling.
    #[test]
    fn payment_cltv_limit_defaults_clear_the_route_floor() {
        let defaults = crate::config::types::LightningSettings::default();
        assert!(
            defaults.payment_cltv_limit
                >= defaults.max_final_cltv_expiry_delta + MIN_ROUTE_CLTV_HEADROOM,
            "default payment_cltv_limit must not need deferring"
        );
        assert_eq!(
            payment_cltv_limit_blocks(
                defaults.payment_cltv_limit,
                defaults.max_final_cltv_expiry_delta
            ),
            defaults.payment_cltv_limit as i32
        );
    }

    /// The default must also fit under a stock LND, whose `--max-cltv-expiry`
    /// is 2016: LND refuses a larger `cltv_limit` outright, so a default above
    /// it would break every payout on an untouched node.
    #[test]
    fn payment_cltv_limit_default_fits_under_a_stock_lnd_maximum() {
        const LND_DEFAULT_MAX_CLTV_EXPIRY: u32 = 2016;
        let defaults = crate::config::types::LightningSettings::default();
        assert!(defaults.payment_cltv_limit <= LND_DEFAULT_MAX_CLTV_EXPIRY);
    }

    /// `0` means "send no ceiling", the documented escape hatch for a node
    /// whose own `--max-cltv-expiry` sits below what Mostro would send.
    #[test]
    fn payment_cltv_limit_zero_defers_to_the_node() {
        assert_eq!(payment_cltv_limit_blocks(0, 144), 0);
        assert_eq!(payment_cltv_limit_blocks(0, u32::MAX), 0);
    }

    /// A ceiling with no room for a route would reject every path an honest
    /// wallet produces. It defers to the node rather than being raised to the
    /// floor: LND rejects a `cltv_limit` above its own `--max-cltv-expiry`
    /// outright, so inventing a replacement could trade "no route" for "every
    /// payout rejected".
    #[test]
    fn payment_cltv_limit_defers_a_ceiling_that_starves_the_route() {
        assert_eq!(payment_cltv_limit_blocks(100, 144), 0);
        // Exactly one block short still defers; exactly at the floor is kept.
        let floor = 144 + MIN_ROUTE_CLTV_HEADROOM;
        assert_eq!(payment_cltv_limit_blocks(floor - 1, 144), 0);
        assert_eq!(payment_cltv_limit_blocks(floor, 144), floor as i32);
    }

    /// `cltv_limit` is an i32 on the wire. An absurd configuration must
    /// saturate, never wrap into a negative — which LND would read as a
    /// ceiling far tighter than the operator asked for.
    #[test]
    fn payment_cltv_limit_saturates_instead_of_wrapping_negative() {
        assert_eq!(payment_cltv_limit_blocks(u32::MAX, 144), i32::MAX);
        assert!(payment_cltv_limit_blocks(u32::MAX, u32::MAX) >= 0);
    }

    /// The retry that saves an operator whose node caps timelocks lower than
    /// Mostro asks for only fires on LND's own complaint about the ceiling —
    /// never on an unrelated failure, which must surface as the error it is.
    #[test]
    fn cltv_limit_rejection_is_recognised_only_for_the_ceiling() {
        assert!(is_cltv_limit_rejection(
            "cltv limit 1008 should be less than 500"
        ));
        assert!(is_cltv_limit_rejection("CLTV limit exceeds maximum"));
        assert!(!is_cltv_limit_rejection("no route to destination"));
        assert!(!is_cltv_limit_rejection("insufficient local balance"));
        assert!(!is_cltv_limit_rejection(""));
    }

    #[test]
    fn small_amounts_use_one_percent_with_ten_sat_floor() {
        init_test_settings();
        // At and below 1000 sats the floor of 10 dominates the 1% rate,
        // independent of `max_routing_fee`.
        assert_eq!(routing_fee_cap_sats(30), 10);
        assert_eq!(routing_fee_cap_sats(500), 10);
        assert_eq!(routing_fee_cap_sats(1000), 10);
    }

    #[test]
    fn large_amounts_use_max_routing_fee_truncated() {
        init_test_settings();
        // Above 1000 sats the cap is `amount * max_routing_fee`, truncated
        // (not rounded up) to match LND's `fee_limit_sat`.
        assert_eq!(routing_fee_cap_sats(1001), 2); // 2.002 -> 2
        assert_eq!(routing_fee_cap_sats(2001), 4); // 4.002 -> 4
        assert_eq!(routing_fee_cap_sats(100_000), 200);
    }

    // --- decode_hash32 -----------------------------------------------
    //
    // These guard the CRITICAL fix for #804: a malformed `preimage` /
    // `hash` column must fail *that* operation, never panic the daemon.

    /// Assert the error is the typed hold-invoice error naming `field`,
    /// and that it never echoes the raw value back into logs.
    fn assert_hold_invoice_err(err: MostroError, field: &str, value: &str) {
        match err {
            MostroInternalErr(ServiceError::HoldInvoiceError(msg)) => {
                assert!(
                    msg.contains(field),
                    "error should name the offending column, got: {msg}"
                );
                assert!(
                    !msg.contains(value),
                    "error must not leak the secret value, got: {msg}"
                );
            }
            other => panic!("expected HoldInvoiceError, got {other:?}"),
        }
    }

    #[test]
    fn decodes_valid_32_byte_hex() {
        // Arrange
        let value = "ab".repeat(32);

        // Act
        let bytes = decode_hash32("preimage", &value).expect("valid hex must decode");

        // Assert
        assert_eq!(bytes, vec![0xab; 32]);
    }

    #[test]
    fn accepts_uppercase_hex() {
        // Arrange: LND and some tooling emit uppercase hex; rejecting it
        // would strand otherwise-valid rows.
        let value = "AB".repeat(32);

        // Act
        let bytes = decode_hash32("preimage", &value).expect("uppercase hex must decode");

        // Assert
        assert_eq!(bytes, vec![0xab; 32]);
    }

    #[test]
    fn returns_error_instead_of_panicking_on_non_hex() {
        // Arrange: `bonds` fixtures and hand-edited rows have produced
        // values like this; before #804 they panicked the process.
        let value = "p".repeat(64);

        // Act
        let err = decode_hash32("preimage", &value).expect_err("non-hex must be rejected");

        // Assert
        assert_hold_invoice_err(err, "preimage", &value);
    }

    #[test]
    fn returns_error_on_odd_length_hex() {
        // Arrange: a truncated / partially written column.
        let value = "abc";

        // Act
        let err = decode_hash32("payment hash", value).expect_err("odd length must be rejected");

        // Assert
        assert_hold_invoice_err(err, "payment hash", value);
    }

    #[test]
    fn returns_error_on_empty_string() {
        // Arrange / Act
        let err = decode_hash32("preimage", "").expect_err("empty must be rejected");

        // Assert: empty is valid hex for a zero-length Vec, so it is the
        // length check that has to catch it.
        match err {
            MostroInternalErr(ServiceError::HoldInvoiceError(msg)) => {
                assert!(msg.contains("expected 32 bytes, got 0"), "got: {msg}")
            }
            other => panic!("expected HoldInvoiceError, got {other:?}"),
        }
    }

    #[test]
    fn returns_error_on_wrong_length_hex() {
        // Arrange: well-formed hex, wrong size — LND would reject it
        // anyway, but we want a clear error at the boundary.
        for value in ["00".repeat(31), "00".repeat(33)] {
            // Act
            let err = decode_hash32("preimage", &value).expect_err("wrong length must be rejected");

            // Assert
            assert_hold_invoice_err(err, "preimage", &value);
        }
    }
}

#[cfg(test)]
mod offline_connector_tests {
    //! `fedimint_tonic_lnd::connect` is lazy: it reads the TLS cert and
    //! macaroon files and builds a channel, but never touches the network
    //! until the first RPC. That lets these tests construct a real
    //! `LndConnector` pointed at a dead localhost port and exercise every
    //! RPC method's transport-error path without a live LND node.
    use super::*;
    use crate::config::MOSTRO_CONFIG;
    use fedimint_tonic_lnd::lnrpc::GetInfoResponse;

    fn init_test_settings() {
        crate::config::init_test_nostr_keys();
        let _ = MOSTRO_CONFIG.set(crate::app::context::test_utils::test_settings());
    }

    /// Build a connector whose channel points at a closed localhost port.
    /// Empty cert (rustls_pemfile yields zero certs) and empty macaroon are
    /// both accepted by the lazy connector.
    async fn offline_connector() -> LndConnector {
        let dir = std::env::temp_dir().join(format!("mostro-lnd-offline-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let cert = dir.join("tls.cert");
        let macaroon = dir.join("admin.macaroon");
        std::fs::write(&cert, b"").expect("write cert");
        std::fs::write(&macaroon, b"").expect("write macaroon");
        let client = fedimint_tonic_lnd::connect("https://127.0.0.1:1".to_string(), cert, macaroon)
            .await
            .expect("lazy connect must not touch the network");
        LndConnector { client }
    }

    /// Amount-carrying regtest invoice (500u = 50_000 sats), reused from the
    /// `lightning::invoice` test fixtures.
    const INVOICE_500U: &str = "lnbcrt500u1p3lzwdzpp5t9kgwgwd07y2lrwdscdnkqu4scrcgpm5pt9uwx0rxn5rxawlxlvqdqqcqzpgxqyz5vqsp5a6k7syfxeg8jy63rteywwjla5rrg2pvhedx8ajr2ltm4seydhsqq9qyyssq0n2uwlumsx4d0mtjm8tp7jw3y4da6p6z9gyyjac0d9xugf72lhh4snxpugek6n83geafue9ndgrhuhzk98xcecu2t3z56ut35mkammsqscqp0n";

    #[tokio::test]
    async fn new_fails_without_reachable_files() {
        init_test_settings();
        // Default LightningSettings point at empty paths: the cert read
        // fails before any network activity, so `new` errors cleanly.
        let result = LndConnector::new().await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn create_hold_invoice_surfaces_transport_error() {
        init_test_settings();
        let mut ln = offline_connector().await;
        let res = ln.create_hold_invoice("test hold invoice", 1_000).await;
        assert!(res.is_err());
    }

    #[tokio::test]
    async fn settle_hold_invoice_surfaces_transport_error() {
        init_test_settings();
        let mut ln = offline_connector().await;
        let preimage = "aa".repeat(32);
        assert!(ln.settle_hold_invoice(&preimage).await.is_err());
    }

    #[tokio::test]
    async fn cancel_hold_invoice_error_carries_grpc_code_prefix() {
        init_test_settings();
        let mut ln = offline_connector().await;
        let hash = "bb".repeat(32);
        let err = ln
            .cancel_hold_invoice(&hash)
            .await
            .expect_err("dead port must error");
        // Bond release parses the stable `code=<Code>` prefix; pin it.
        assert!(
            err.to_string().contains("code="),
            "error must carry the code= prefix, got: {err}"
        );
    }

    #[tokio::test]
    async fn subscribe_invoice_surfaces_transport_error() {
        init_test_settings();
        let mut ln = offline_connector().await;
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        assert!(ln.subscribe_invoice(vec![0u8; 32], tx).await.is_err());
    }

    #[tokio::test]
    async fn get_node_info_surfaces_transport_error() {
        init_test_settings();
        let mut ln = offline_connector().await;
        assert!(ln.get_node_info().await.is_err());
    }

    #[tokio::test]
    async fn lookup_payment_status_maps_transport_error() {
        init_test_settings();
        let mut ln = offline_connector().await;
        let err = ln
            .lookup_payment_status(&[0u8; 32])
            .await
            .expect_err("transport failure must be Err, not Ok(None)");
        assert!(err.to_string().contains("code="));
    }

    #[tokio::test]
    async fn check_payment_status_surfaces_transport_error() {
        init_test_settings();
        let mut ln = offline_connector().await;
        assert!(ln.check_payment_status(&[0u8; 32]).await.is_err());
    }

    #[tokio::test]
    async fn send_payment_rejects_wrong_amount_before_paying() {
        init_test_settings();
        let mut ln = offline_connector().await;
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        // Invoice is 50_000 sats; passing 100 must abort with Wrong amount.
        let err = ln
            .send_payment(INVOICE_500U, 100, tx)
            .await
            .expect_err("wrong amount must be rejected");
        assert!(err.to_string().contains("Wrong amount"));
    }

    #[tokio::test]
    async fn send_payment_with_matching_amount_fails_on_transport() {
        init_test_settings();
        let mut ln = offline_connector().await;
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        // Amount matches the invoice, so the failure comes from the dead
        // port at send_payment_v2 time.
        assert!(ln.send_payment(INVOICE_500U, 50_000, tx).await.is_err());
    }

    #[test]
    fn ln_status_maps_get_info_response_fields() {
        let info = GetInfoResponse {
            version: "0.18.0-beta".to_string(),
            identity_pubkey: "02abc".to_string(),
            commit_hash: "deadbeef".to_string(),
            alias: "test-node".to_string(),
            chains: vec![fedimint_tonic_lnd::lnrpc::Chain {
                chain: "bitcoin".to_string(),
                network: "regtest".to_string(),
            }],
            uris: vec!["02abc@127.0.0.1:9735".to_string()],
            ..Default::default()
        };
        let status = LnStatus::from_get_info_response(info);
        assert_eq!(status.version, "0.18.0-beta");
        assert_eq!(status.node_pubkey, "02abc");
        assert_eq!(status.commit_hash, "deadbeef");
        assert_eq!(status.node_alias, "test-node");
        assert_eq!(status.chains, vec!["bitcoin".to_string()]);
        assert_eq!(status.networks, vec!["regtest".to_string()]);
        assert_eq!(status.uris, vec!["02abc@127.0.0.1:9735".to_string()]);
    }
}
