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
use mostro_core::error::{
    MostroError::{self, *},
    ServiceError,
};
use nostr_sdk::nostr::hashes::hex::FromHex;
use nostr_sdk::nostr::secp256k1::rand::{self, RngCore};
use std::cmp::Ordering;
use tokio::sync::mpsc::Sender;
use tracing::info;

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
        let preimage = FromHex::from_hex(preimage).expect("Wrong preimage");

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
            Err(e) => Err(MostroInternalErr(ServiceError::LnNodeError(e.to_string()))),
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
