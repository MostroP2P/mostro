pub mod invoice;

use crate::config::settings::Settings;
use crate::lightning::invoice::decode_invoice;
use crate::util::bytes_to_string;
use easy_hasher::easy_hasher::*;
use fedimint_tonic_lnd::invoicesrpc::{
    AddHoldInvoiceRequest, AddHoldInvoiceResp, CancelInvoiceMsg, CancelInvoiceResp,
    SettleInvoiceMsg, SettleInvoiceResp,
};
use fedimint_tonic_lnd::lnrpc::{invoice::InvoiceState, GetInfoRequest, GetInfoResponse, Payment};
use fedimint_tonic_lnd::routerrpc::{SendPaymentRequest, TrackPaymentRequest};
use fedimint_tonic_lnd::Client;
use mostro_core::prelude::*;
use nostr_sdk::nostr::hashes::hex::FromHex;
use nostr_sdk::nostr::secp256k1::rand::{self, RngCore};
use std::cmp::Ordering;
use tokio::sync::mpsc::Sender;
use tracing::info;

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
fn decode_hash32(field: &str, value: &str) -> Result<Vec<u8>, MostroError> {
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

    pub async fn send_payment(
        &mut self,
        payment_request: &str,
        amount: i64,
        listener: Sender<PaymentMessage>,
    ) -> Result<(), MostroError> {
        let invoice = decode_invoice(payment_request)?;
        let payment_hash = invoice.signable_hash();
        let hash = bytes_to_string(&payment_hash);

        // We need to set a max fee amount. `routing_fee_cap_sats` is the
        // single source of truth so the value persisted for operator
        // debugging always matches what LND actually enforces.
        let max_fee = routing_fee_cap_sats(amount);

        let track_payment_req = TrackPaymentRequest {
            payment_hash: payment_hash.to_vec(),
            no_inflight_updates: true,
        };

        let track = self
            .client
            .router()
            .track_payment_v2(track_payment_req)
            .await
            .map_err(|e| MostroInternalErr(ServiceError::LnPaymentError(e.to_string())));

        // We only send the payment if it wasn't attempted before
        if track.is_ok() {
            info!("Aborting paying invoice with hash {} to buyer", hash);
            return Err(MostroInternalErr(ServiceError::LnPaymentError(
                "Track error".to_string(),
            )));
        }

        let mut request = SendPaymentRequest {
            payment_request: payment_request.to_string(),
            timeout_seconds: 60,
            fee_limit_sat: max_fee,
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

        let outer_stream = self
            .client
            .router()
            .send_payment_v2(request)
            .await
            .map_err(|e| MostroInternalErr(ServiceError::LnPaymentError(e.to_string())));

        // We can safely unwrap here cause await was successful
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
    use super::{decode_hash32, routing_fee_cap_sats};
    use crate::config::settings::Settings;
    use crate::config::MOSTRO_CONFIG;
    use mostro_core::prelude::*;

    fn init_test_settings() {
        // Defaults set `max_routing_fee = 0.002`.
        let _ = MOSTRO_CONFIG.set(Settings {
            database: Default::default(),
            nostr: crate::config::NostrSettings {
                nsec_privkey: "nsec13as48eum93hkg7plv526r9gjpa0uc52zysqm93pmnkca9e69x6tsdjmdxd"
                    .to_string(),
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
