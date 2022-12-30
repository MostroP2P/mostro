use easy_hasher::easy_hasher::*;
use nostr::hashes::hex::FromHex;
use nostr::key::FromBech32;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use std::env;
use tonic_openssl_lnd::invoicesrpc::{
    AddHoldInvoiceRequest, AddHoldInvoiceResp, SettleInvoiceMsg, SettleInvoiceResp,
};
use tonic_openssl_lnd::lnrpc::invoice::InvoiceState;
use tonic_openssl_lnd::{LndClient, LndClientError};

pub struct LndConnector {
    client: LndClient,
}

#[derive(Debug, Clone)]
pub struct InvoiceMessage {
    pub hash: Vec<u8>,
    pub state: InvoiceState,
}

impl LndConnector {
    pub async fn new() -> Self {
        let port: u32 = env::var("LND_GRPC_PORT")
            .expect("LND_GRPC_PORT must be set")
            .parse()
            .expect("port is not u32");
        let host = env::var("LND_GRPC_HOST").expect("LND_GRPC_HOST must be set");
        let tls_path = env::var("LND_CERT_FILE").expect("LND_CERT_FILE must be set");
        let macaroon_path = env::var("LND_MACAROON_FILE").expect("LND_MACAROON_FILE must be set");

        // Connecting to LND requires only host, port, cert file, and macaroon file
        let client = tonic_openssl_lnd::connect(host, port, tls_path, macaroon_path)
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

        let invoice = AddHoldInvoiceRequest {
            hash: hash.to_vec(),
            memo: description.to_string(),
            value: amount,
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

    pub async fn subscribe_invoice(
        &mut self,
        r_hash: Vec<u8>,
        listener: tokio::sync::mpsc::Sender<InvoiceMessage>,
    ) {
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

        let preimage = SettleInvoiceMsg { preimage };
        let settle = self
            .client
            .invoices()
            .settle_invoice(preimage)
            .await
            .expect("Failed to add hold invoice")
            .into_inner();

        Ok(settle)
    }
}
