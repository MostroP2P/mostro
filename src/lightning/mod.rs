pub mod invoice;
use std::cmp::Ordering;

use crate::cli::settings::Settings;
use crate::error::MostroError;
use crate::lightning::invoice::decode_invoice;
use crate::util::bytes_to_string;

use anyhow::Result;
use easy_hasher::easy_hasher::*;
use fedimint_tonic_lnd::tonic::Status;
use nostr_sdk::nostr::hashes::hex::FromHex;
use nostr_sdk::nostr::secp256k1::rand::{self, RngCore};
use tokio::sync::mpsc::Sender;
use fedimint_tonic_lnd::lnrpc::{AddInvoiceResponse, InvoiceSubscription};
use fedimint_tonic_lnd::lnrpc::{invoice::InvoiceState, Payment, Invoice};
use fedimint_tonic_lnd::invoicesrpc::{AddHoldInvoiceRequest, AddHoldInvoiceResp};
use fedimint_tonic_lnd::{ConnectError, LightningClient};
use fedimint_tonic_lnd::{Client,};
use tracing::info;
// use tonic_lnd::lnrpc::


pub struct LndConnector {
    client: Client,
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
    pub async fn new() -> anyhow::Result<Self> {
        let ln_settings = Settings::get_ln();

        // Connecting to LND requires only host, port, cert file, and macaroon file
        let client = fedimint_tonic_lnd::connect(
            ln_settings.lnd_grpc_host,
            ln_settings.lnd_cert_file,
            ln_settings.lnd_macaroon_file,
        )
        .await
        .map_err(|e| MostroError::LnNodeError(e.to_string()))?;

        // Safe unwrap here
        Ok(Self { client })
    }

    pub async fn create_hold_invoice(
        &mut self,
        description: &str,
        amount: i64,
    ) -> Result<(AddHoldInvoiceResp, Vec<u8>, Vec<u8>), String > {
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
            .map_err(|e| e);

        match holdinvoice {
            Ok(holdinvoice) => Ok((holdinvoice.into_inner(), preimage.to_vec(), hash.to_vec())),
            Err(e) => Err(e.message().to_string()),
        }
    }

    pub async fn subscribe_invoice(
        &mut self,
        r_hash: Vec<u8>,
        listener: Sender<InvoiceState>,
    ) -> anyhow::Result<()> {
        let invoice_stream = self
            .client.lightning()
            .subscribe_invoices(fedimint_tonic_lnd::lnrpc::InvoiceSubscription {
                add_index: 0,
                settle_index: 0,
            })
            .await
            .map_err(|e| MostroError::LnNodeError(e.to_string()))?;

        let mut inner_invoice = invoice_stream.into_inner();

        while let Some(invoice) = inner_invoice
            .message()
            .await
            .map_err(|e| MostroError::LnNodeError(e.to_string()))?
        {
            if let Some(state) =
            fedimint_tonic_lnd::lnrpc::invoice::InvoiceState::try_from(invoice.state)
            {
                let msg = InvoiceMessage {
                    hash: r_hash.clone(),
                    state,
                };
                listener
                    .clone()
                    .send(state)
                    .await
                    .map_err(|e| MostroError::LnNodeError(e.to_string()))?
            }
        }
        Ok(())
    }

    pub async fn settle_hold_invoice(
        &mut self,
        preimage: &str,
    ) -> Result<SettleInvoiceResp, ConnectError> {
        let preimage = FromHex::from_hex(preimage).expect("Wrong preimage");

        let preimage_message = SettleInvoiceMsg { preimage };
        let settle = self
            .client
            .lightning().se

            // .invoices()
            // .settle_invoice(preimage_message)
            .await
            .map_err(|e| e.to_string());

        match settle {
            Ok(settle) => Ok(settle.into_inner()),
            Err(e) => Err(e),
        }
    }

    pub async fn cancel_hold_invoice(
        &mut self,
        hash: &str,
    ) -> Result<CancelInvoiceResp, ConnectError> {
        let payment_hash = FromHex::from_hex(hash).expect("Wrong payment hash");

        let cancel_message = CancelInvoiceMsg { payment_hash };
        let cancel = self
            .client
            .invoices()
            .cancel_invoice(cancel_message)
            .await
            .map_err(|e| e.to_string());

        match cancel {
            Ok(cancel) => Ok(cancel.into_inner()),
            Err(e) => Err(e),
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
        let mostro_settings = Settings::get_mostro();

        // We need to set a max fee amount
        // If the amount is small we use a different max routing fee
        let max_fee = match amount.cmp(&1000) {
            Ordering::Less | Ordering::Equal => amount as f64 * 0.01,
            Ordering::Greater => amount as f64 * mostro_settings.max_routing_fee,
        };

        let track_payment_req = TrackPaymentRequest {
            payment_hash: payment_hash.to_vec(),
            no_inflight_updates: true,
        };

        let track = self
            .client
            .router()
            .track_payment_v2(track_payment_req)
            .await
            .map_err(|e| MostroError::LnPaymentError(e.to_string()));

        // We only send the payment if it wasn't attempted before
        if track.is_ok() {
            info!("Aborting paying invoice with hash {} to buyer", hash);
            return Err(MostroError::LnPaymentError("Track error".to_string()));
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
                    return Err(MostroError::LnPaymentError("Wrong amount".to_string()));
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
            .map_err(|e| MostroError::LnPaymentError(e.to_string()));

        // We can safely unwrap here cause await was successful
        let mut stream = outer_stream
            .map_err(|e| MostroError::LnPaymentError(e.to_string()))?
            .into_inner();

        while let Ok(Some(payment)) = stream
            .message()
            .await
            .map_err(|e| MostroError::LnPaymentError(e.to_string()))
        {
            //   ("Failed paying invoice") {
            let msg = PaymentMessage { payment };
            listener
                .clone()
                .send(msg)
                .await
                .map_err(|e| MostroError::LnNodeError(e.to_string()))?
        }

        Ok(())
    }
}
