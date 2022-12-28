use easy_hasher::easy_hasher::*;
use log::info;
use nostr::hashes::hex::{FromHex, ToHex};
use nostr::key::FromBech32;
use rand::RngCore;
use std::env;
use tonic_openssl_lnd::invoicesrpc::{
    AddHoldInvoiceRequest, AddHoldInvoiceResp, SettleInvoiceMsg, SettleInvoiceResp,
};
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
    let mut client = connect().await.expect("failed to connect lightning node");
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

pub async fn subscribe_invoice(hash: &str) {
    let mut client = connect().await.expect("failed to connect lightning node");
    let nostr_client = crate::util::connect_nostr().await.unwrap();
    let pool = crate::db::connect().await.unwrap();
    let hash = FromHex::from_hex(hash).expect("Wrong hash");
    let mut invoice_stream = client
        .invoices()
        .subscribe_single_invoice(
            tonic_openssl_lnd::invoicesrpc::SubscribeSingleInvoiceRequest { r_hash: hash },
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
            let hash = invoice.r_hash.to_hex();
            let order = crate::db::find_order_by_hash(&pool, &hash).await.unwrap();
            let my_keys = crate::util::get_keys().unwrap();
            let seller_pubkey = order.seller_pubkey.as_ref().unwrap();
            let seller_keys = nostr::key::Keys::from_bech32_public_key(seller_pubkey).unwrap();
            let buyer_pubkey = order.buyer_pubkey.as_ref().unwrap();
            let buyer_keys = nostr::key::Keys::from_bech32_public_key(buyer_pubkey).unwrap();
            // If this invoice was paid by the seller
            if state == tonic_openssl_lnd::lnrpc::invoice::InvoiceState::Accepted {
                info!(
                    "Order Id: {} - Seller paid invoice with hash: {hash}",
                    order.id
                );
                // We publish a new kind 11000 nostr event with the status updated
                // and update on local database the status and new event id
                // crate::util::update_order_event(
                //     &pool,
                //     &nostr_client,
                //     &my_keys,
                //     crate::types::Status::Active,
                //     &order,
                // )
                // .await
                // .unwrap();
                // We send a confirmation message to seller
                let message = crate::messages::buyer_took_order(&order, buyer_pubkey);
                crate::util::send_dm(&nostr_client, &my_keys, &seller_keys, message)
                    .await
                    .unwrap();
                // We send a message to buyer saying seller paid
                let message = crate::messages::get_in_touch_with_seller(&order, seller_pubkey);
                crate::util::send_dm(&nostr_client, &my_keys, &buyer_keys, message)
                    .await
                    .unwrap();
            } else if state == tonic_openssl_lnd::lnrpc::invoice::InvoiceState::Settled {
                // If this invoice was Settled we can do something with it
                info!(
                    "Order Id: {} - Seller released funds for invoice hash: {hash}",
                    order.id
                );

                // We publish a new kind 11000 nostr event with the status updated
                // and update on local database the status and new event id
                // crate::util::update_order_event(
                //     &pool,
                //     &nostr_client,
                //     &my_keys,
                //     crate::types::Status::SettledInvoice,
                //     &order,
                // )
                // .await
                // .unwrap();
                // We send a *funds released* message to seller
                let message = crate::messages::sell_success(buyer_pubkey);
                crate::util::send_dm(&nostr_client, &my_keys, &seller_keys, message)
                    .await
                    .unwrap();
                // We send a message to buyer saying seller released
                let message = crate::messages::funds_released(seller_pubkey);
                crate::util::send_dm(&nostr_client, &my_keys, &buyer_keys, message)
                    .await
                    .unwrap();
            } else if state == tonic_openssl_lnd::lnrpc::invoice::InvoiceState::Canceled {
                // If this invoice was Canceled
                info!(
                    "Order Id: {} - Invoice with hash: {hash} was canceled!",
                    order.id
                );
                // We publish a new kind 11000 nostr event with the status updated
                // and update on local database the status and new event id
                // crate::util::update_order_event(
                //     &pool,
                //     &nostr_client,
                //     &my_keys,
                //     crate::types::Status::Canceled,
                //     &order,
                // )
                // .await
                // .unwrap();
                // We send "order canceled" messages to both parties
                let message = crate::messages::order_canceled(order.id);
                crate::util::send_dm(&nostr_client, &my_keys, &seller_keys, message.clone())
                    .await
                    .unwrap();
                crate::util::send_dm(&nostr_client, &my_keys, &buyer_keys, message)
                    .await
                    .unwrap();
            } else {
                info!(
                    "Order Id: {} - Invoice with hash: {hash} subscribed!",
                    order.id
                );
            }
        }
    }
}

pub async fn settle_hold_invoice(preimage: &str) -> Result<SettleInvoiceResp, LndClientError> {
    let preimage = FromHex::from_hex(preimage).expect("Wrong preimage");
    let mut client = connect().await.expect("failed to connect lightning node");

    let preimage = SettleInvoiceMsg { preimage };
    let settle = client
        .invoices()
        .settle_invoice(preimage)
        .await
        .expect("Failed to add hold invoice")
        .into_inner();

    Ok(settle)
}
