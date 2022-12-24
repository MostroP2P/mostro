use easy_hasher::easy_hasher::*;
use rand::RngCore;
use std::env;
use tonic_openssl_lnd::invoicesrpc::{AddHoldInvoiceRequest, AddHoldInvoiceResp};
use tonic_openssl_lnd::{LndClient, LndClientError};

pub async fn connect() -> Result<LndClient, LndClientError> {
    let port: u32 = env::var("LND_GRPC_PORT")
        .expect("LND_GRPC_PORT must be set")
        .parse()
        .expect("port is not u32");
    let host = env::var("LND_GRPC_HOST").expect("LND_GRPC_HOST must be set");
    let cert = env::var("LND_CERT_FILE").expect("LND_CERT_FILE must be set");
    let macaroon = env::var("LND_MACAROON_FILE").expect("LND_MACAROON_FILE must be set");
    // Connecting to LND requires only host, port, cert file, and macaroon file
    let client = tonic_openssl_lnd::connect(host, port, cert, macaroon)
        .await
        .expect("Failed connecting to LND");

    Ok(client)
}

pub async fn create_hold_invoice(
    description: &str,
    amount: i64,
) -> Result<(AddHoldInvoiceResp, Vec<u8>, Vec<u8>), LndClientError> {
    let mut client = connect().await.unwrap();
    let mut preimage = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut preimage);
    let hash = raw_sha256(preimage.to_vec());

    let invoice = AddHoldInvoiceRequest {
        hash: hash.to_vec(),
        memo: description.to_string(),
        value: amount,
        ..Default::default()
    };
    let holdinvoice = client
        .invoices()
        .add_hold_invoice(invoice)
        .await
        .expect("Failed to add hold invoice")
        .into_inner();

    Ok((holdinvoice, preimage.to_vec(), hash.to_vec()))
}
