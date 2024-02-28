pub mod invoice;
use std::cmp::Ordering;

use crate::cli::settings::Settings;
use crate::lightning::invoice::decode_invoice;
use crate::util::bytes_to_string;

use anyhow::Result;
use easy_hasher::easy_hasher::*;
use nostr_sdk::nostr::hashes::hex::FromHex;
use nostr_sdk::nostr::secp256k1::rand::{self, RngCore};
use tokio::sync::mpsc::Sender;
use tonic_openssl_lnd::invoicesrpc::{
    AddHoldInvoiceRequest, AddHoldInvoiceResp, CancelInvoiceMsg, CancelInvoiceResp,
    SettleInvoiceMsg, SettleInvoiceResp,
};
use tonic_openssl_lnd::lnrpc::{invoice::InvoiceState, Payment};
use tonic_openssl_lnd::routerrpc::{SendPaymentRequest, TrackPaymentRequest};
use tonic_openssl_lnd::{LndClient, LndClientError};
use tracing::info;

pub struct LndConnector {
    client: LndClient,
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

impl LndConnector {
    pub async fn new() -> Self {
        let ln_settings = Settings::get_ln();

        // Connecting to LND requires only host, port, cert file, and macaroon file
        let client = tonic_openssl_lnd::connect(
            ln_settings.lnd_grpc_host,
            ln_settings.lnd_grpc_port,
            ln_settings.lnd_cert_file,
            ln_settings.lnd_macaroon_file,
        )
        .await
        .expect("Failed connecting to LND");

        Self { client }
    }

    pub async fn create_hold_invoice(
        &mut self,
        description: &str,
        amount: i64,
    ) -> Result<(AddHoldInvoiceResp, Vec<u8>, Vec<u8>), LndClientError> {
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
            .expect("Failed to add hold invoice")
            .into_inner();

        Ok((holdinvoice, preimage.to_vec(), hash.to_vec()))
    }

    pub async fn subscribe_invoice(&mut self, r_hash: Vec<u8>, listener: Sender<InvoiceMessage>) {
        let mut invoice_stream = self
            .client
            .invoices()
            .subscribe_single_invoice(
                tonic_openssl_lnd::invoicesrpc::SubscribeSingleInvoiceRequest {
                    r_hash: r_hash.clone(),
                },
            )
            .await
            .expect("Failed to call subscribe_single_invoice")
            .into_inner();

        while let Some(invoice) = invoice_stream
            .message()
            .await
            .expect("Failed to receive invoices")
        {
            if let Some(state) =
                tonic_openssl_lnd::lnrpc::invoice::InvoiceState::from_i32(invoice.state)
            {
                let msg = InvoiceMessage {
                    hash: r_hash.clone(),
                    state,
                };
                listener
                    .clone()
                    .send(msg)
                    .await
                    .expect("Failed to send a message");
            }
        }
    }

    pub async fn settle_hold_invoice(
        &mut self,
        preimage: &str,
    ) -> Result<SettleInvoiceResp, LndClientError> {
        let preimage = FromHex::from_hex(preimage).expect("Wrong preimage");

        let preimage_message = SettleInvoiceMsg { preimage };
        let settle = self
            .client
            .invoices()
            .settle_invoice(preimage_message)
            .await
            .expect("Failed to settle hold invoice")
            .into_inner();

        Ok(settle)
    }

    pub async fn cancel_hold_invoice(
        &mut self,
        hash: &str,
    ) -> Result<CancelInvoiceResp, LndClientError> {
        let payment_hash = FromHex::from_hex(hash).expect("Wrong payment hash");

        let cancel_message = CancelInvoiceMsg { payment_hash };
        let cancel = self
            .client
            .invoices()
            .cancel_invoice(cancel_message)
            .await
            .expect("Failed to cancel hold invoice")
            .into_inner();

        Ok(cancel)
    }

    pub async fn send_payment(
        &mut self,
        payment_request: &str,
        amount: i64,
        listener: Sender<PaymentMessage>,
    ) {
        let invoice = decode_invoice(payment_request).unwrap();
        let payment_hash = invoice.payment_hash();
        let payment_hash = payment_hash.to_vec();
        let hash = bytes_to_string(&payment_hash);
        let mostro_settings = Settings::get_mostro();

        // We need to set a max fee amount
        // If the amount is small we use a different max routing fee
        let max_fee = match amount.cmp(&100) {
            Ordering::Less | Ordering::Equal => amount as f64 * 0.1,
            Ordering::Greater => amount as f64 * mostro_settings.max_routing_fee,
        };

        let track_payment_req = TrackPaymentRequest {
            payment_hash,
            no_inflight_updates: true,
        };

        let track = self
            .client
            .router()
            .track_payment_v2(track_payment_req)
            .await;

        // We only send the payment if it wasn't attempted before
        if track.is_ok() {
            info!("Aborting paying invoice with hash {} to buyer", hash);
            return;
        }

        let mut request = SendPaymentRequest {
            payment_request: payment_request.to_string(),
            timeout_seconds: 60,
            fee_limit_sat: max_fee as i64,
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
                    return;
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

        let mut stream = self
            .client
            .router()
            .send_payment_v2(request)
            .await
            .expect("Failed sending payment")
            .into_inner();

        while let Some(payment) = stream.message().await.expect("Failed paying invoice") {
            let msg = PaymentMessage { payment };
            listener
                .clone()
                .send(msg)
                .await
                .expect("Failed to send a message");
        }
    }
}
