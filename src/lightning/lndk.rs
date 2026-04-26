//! LNDK gRPC client for BOLT12 offer payouts.
//!
//! LNDK (<https://github.com/lndk-org/lndk>) is a daemon that sits beside
//! LND and implements BOLT12 using LDK's offers code while delegating
//! Lightning routing and onion message forwarding to LND. Mostro talks to
//! LNDK over its gRPC API to fetch invoices from offers and pay them.
//!
//! Only the buyer-payout path uses this. Hold invoices, seller payments,
//! dev-fee payouts, and LND health checks all continue to use
//! [`super::LndConnector`] directly.
//!
//! # Two-step fetch-then-pay
//!
//! LNDK exposes a convenient `PayOffer` RPC that fetches and pays in one
//! call, but it does not verify the returned invoice's amount or expiry
//! against what the caller asked for. We therefore use `GetInvoice` →
//! defensive validation (see [`super::offers::validate_fetched_invoice`]) →
//! `PayInvoice` so a misbehaving offer issuer cannot slip an over-long or
//! zero-amount invoice past us.

use crate::config::settings::Settings;
use crate::lightning::offers::validate_fetched_invoice;
use mostro_core::prelude::*;
use tonic::metadata::MetadataValue;
use tonic::transport::{Certificate, Channel, ClientTlsConfig};
use tonic::{Request, Status};

/// Generated from `proto/lndkrpc.proto`.
pub mod lndkrpc {
    tonic::include_proto!("lndkrpc");
}

use lndkrpc::offers_client::OffersClient;
use lndkrpc::{GetInvoiceRequest, PayInvoiceRequest};

/// Client handle for the LNDK `Offers` gRPC service.
///
/// Wraps a `tonic::Channel` (HTTP/2 multiplexed, cheap to clone) plus the
/// macaroon + fee configuration needed to authenticate each request.
#[derive(Clone)]
pub struct LndkConnector {
    client: OffersClient<Channel>,
    macaroon_hex: String,
    fetch_timeout_secs: u32,
    fee_limit_percent: u32,
}

impl LndkConnector {
    /// Constructs a connector from the current settings.
    ///
    /// Returns `Ok(None)` when `lightning.lndk_enabled = false`. Returns
    /// `Err` when enabled but the cert/macaroon cannot be loaded or the
    /// TLS channel cannot be opened; callers should abort startup in that
    /// case rather than silently dropping BOLT12 support.
    pub async fn new_from_settings() -> Result<Option<Self>, MostroError> {
        let ln = Settings::get_ln();
        if !ln.lndk_enabled {
            return Ok(None);
        }

        if ln.lndk_cert_file.is_empty() || ln.lndk_macaroon_file.is_empty() {
            return Err(MostroInternalErr(ServiceError::LnNodeError(
                "lndk_enabled=true but lndk_cert_file / lndk_macaroon_file are unset".into(),
            )));
        }

        let cert = tokio::fs::read(&ln.lndk_cert_file).await.map_err(|e| {
            MostroInternalErr(ServiceError::LnNodeError(format!(
                "failed to read LNDK TLS cert {}: {}",
                ln.lndk_cert_file, e
            )))
        })?;

        let macaroon_bytes = tokio::fs::read(&ln.lndk_macaroon_file).await.map_err(|e| {
            MostroInternalErr(ServiceError::LnNodeError(format!(
                "failed to read LNDK macaroon {}: {}",
                ln.lndk_macaroon_file, e
            )))
        })?;
        let macaroon_hex = hex::encode(&macaroon_bytes);

        // LNDK's self-signed cert uses `localhost` as its subject. The
        // configured grpc host may be an IP (127.0.0.1), so we pin the
        // domain name explicitly for SNI + verification.
        let tls = ClientTlsConfig::new()
            .ca_certificate(Certificate::from_pem(&cert))
            .domain_name("localhost");

        let channel = Channel::from_shared(ln.lndk_grpc_host.clone())
            .map_err(|e| {
                MostroInternalErr(ServiceError::LnNodeError(format!(
                    "invalid lndk_grpc_host {}: {}",
                    ln.lndk_grpc_host, e
                )))
            })?
            .tls_config(tls)
            .map_err(|e| MostroInternalErr(ServiceError::LnNodeError(format!("TLS config: {e}"))))?
            .connect()
            .await
            .map_err(|e| {
                MostroInternalErr(ServiceError::LnNodeError(format!(
                    "failed to connect to LNDK at {}: {}",
                    ln.lndk_grpc_host, e
                )))
            })?;

        let mostro_settings = Settings::get_mostro();
        // LNDK's fee_limit_percent is a whole-number percent (e.g. 2 = 2%).
        // `mostro.max_routing_fee` is a fraction (0.002 = 0.2%). Round up to
        // ensure we don't accidentally cap fees below the BOLT11 path.
        let fee_fraction = ln
            .lndk_fee_limit_percent
            .unwrap_or(mostro_settings.max_routing_fee);
        let fee_limit_percent = ((fee_fraction * 100.0).ceil().max(1.0)) as u32;

        Ok(Some(Self {
            client: OffersClient::new(channel),
            macaroon_hex,
            fetch_timeout_secs: ln.lndk_fetch_invoice_timeout,
            fee_limit_percent,
        }))
    }

    /// Pays a BOLT12 offer with a two-step fetch-and-validate flow.
    ///
    /// Returns the payment preimage (hex string) on success.
    pub async fn pay_offer_validated(
        &mut self,
        offer: &str,
        amount_msats: u64,
        payer_note: Option<String>,
        min_expiry_secs: u64,
    ) -> Result<String, MostroError> {
        let mut get_req = Request::new(GetInvoiceRequest {
            offer: offer.to_string(),
            amount: Some(amount_msats),
            payer_note,
            response_invoice_timeout: Some(self.fetch_timeout_secs),
        });
        self.inject_macaroon(&mut get_req)?;

        let fetched = self
            .client
            .get_invoice(get_req)
            .await
            .map_err(|s| map_status(s, "get_invoice"))?
            .into_inner();

        let contents = fetched.invoice_contents.ok_or_else(|| {
            MostroInternalErr(ServiceError::LnPaymentError(
                "LNDK GetInvoice returned empty invoice_contents".into(),
            ))
        })?;

        validate_fetched_invoice(
            contents.amount_msats,
            contents.created_at,
            contents.relative_expiry,
            amount_msats,
            min_expiry_secs,
        )?;

        let mut pay_req = Request::new(PayInvoiceRequest {
            invoice: fetched.invoice_hex_str,
            amount: Some(amount_msats),
            fee_limit: None,
            fee_limit_percent: Some(self.fee_limit_percent),
        });
        self.inject_macaroon(&mut pay_req)?;

        let resp = self
            .client
            .pay_invoice(pay_req)
            .await
            .map_err(|s| map_status(s, "pay_invoice"))?
            .into_inner();

        Ok(resp.payment_preimage)
    }

    fn inject_macaroon<T>(&self, req: &mut Request<T>) -> Result<(), MostroError> {
        let value: MetadataValue<_> = self.macaroon_hex.parse().map_err(|_| {
            MostroInternalErr(ServiceError::LnNodeError(
                "LNDK macaroon is not valid ASCII hex".into(),
            ))
        })?;
        req.metadata_mut().insert("macaroon", value);
        Ok(())
    }
}

/// Maps a tonic `Status` returned by an LNDK RPC into a Mostro error.
///
/// LNDK's handler maps `InvalidArgument` to parse / amount / currency
/// failures (i.e. the offer or request is bad) and routes route /
/// payment / peer / timeout failures through `Internal`. We preserve that
/// split so `check_failure_retries` can distinguish "operator misconfig /
/// user error" from "transient network failure".
fn map_status(status: Status, op: &'static str) -> MostroError {
    use tonic::Code::*;
    match status.code() {
        InvalidArgument => MostroInternalErr(ServiceError::InvoiceInvalidError),
        Unavailable => MostroInternalErr(ServiceError::LnNodeError(format!(
            "lndk {op} unavailable: {}",
            status.message()
        ))),
        _ => MostroInternalErr(ServiceError::LnPaymentError(format!(
            "lndk {op}: {}",
            status.message()
        ))),
    }
}
