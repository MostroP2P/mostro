use crate::cli::settings::Settings;
use crate::util::send_dm;

use log::info;
use mostro_core::order::Status;
use mostro_core::{order::SmallOrder, Action, Content, Message};
use nostr_sdk::prelude::*;

pub async fn hold_invoice_paid(hash: &str) {
    let pool = crate::db::connect().await.unwrap();
    let client = crate::util::connect_nostr().await.unwrap();
    let order = crate::db::find_order_by_hash(&pool, hash).await.unwrap();
    let my_keys = crate::util::get_keys().unwrap();
    let seller_pubkey = XOnlyPublicKey::from_bech32(order.seller_pubkey.as_ref().unwrap()).unwrap();
    let buyer_pubkey = XOnlyPublicKey::from_bech32(order.buyer_pubkey.as_ref().unwrap()).unwrap();

    info!(
        "Order Id: {} - Seller paid invoice with hash: {hash}",
        order.id
    );
    let mut master_buyer_pubkey: Option<String> = None;
    let mut master_seller_pubkey: Option<String> = None;
    // If this is a sell order we show the master identities
    if order.kind == "Sell" {
        master_buyer_pubkey = order.master_buyer_pubkey.clone();
        master_seller_pubkey = order.master_seller_pubkey.clone();
    }

    // We send this data related to the order to the parties
    let mut order_data = SmallOrder::new(
        order.id,
        order.amount,
        order.fiat_code.clone(),
        order.fiat_amount,
        order.payment_method.clone(),
        order.premium,
        master_buyer_pubkey,
        master_seller_pubkey,
    );
    let status;

    if order.buyer_invoice.is_some() {
        // We send a confirmation message to seller
        let message = Message::new(
            0,
            Some(order.id),
            None,
            Action::BuyerTookOrder,
            Some(Content::SmallOrder(order_data.clone())),
        );
        let message = message.as_json().unwrap();
        send_dm(&client, &my_keys, &seller_pubkey, message)
            .await
            .unwrap();
        // We send a message to buyer saying seller paid
        let message = Message::new(
            0,
            Some(order.id),
            None,
            Action::HoldInvoicePaymentAccepted,
            Some(Content::SmallOrder(order_data)),
        );
        let message = message.as_json().unwrap();
        send_dm(&client, &my_keys, &buyer_pubkey, message)
            .await
            .unwrap();
        status = Status::Active;
    } else {
        let mostro_settings = Settings::get_mostro();
        let sub_fee = mostro_settings.fee * order_data.amount as f64;
        let rounded_fee = sub_fee.round();
        let new_amount = order_data.amount - rounded_fee as i64;
        order_data.amount = new_amount;

        // We ask to buyer for a new invoice
        let message = Message::new(
            0,
            Some(order.id),
            None,
            Action::AddInvoice,
            Some(Content::SmallOrder(order_data.clone())),
        );
        let message = message.as_json().unwrap();
        send_dm(&client, &my_keys, &buyer_pubkey, message)
            .await
            .unwrap();
        // We send a message to seller we are waiting for buyer invoice
        let message = Message::new(0, Some(order.id), None, Action::WaitingBuyerInvoice, None);
        let message = message.as_json().unwrap();
        send_dm(&client, &my_keys, &seller_pubkey, message)
            .await
            .unwrap();
        status = Status::WaitingBuyerInvoice;
    }
    // We publish a new replaceable kind nostr event with the status updated
    // and update on local database the status and new event id
    crate::util::update_order_event(&pool, &client, &my_keys, status, &order, None)
        .await
        .unwrap();
    // Update the invoice_held_at field
    crate::db::update_order_invoice_held_at_time(&pool, order.id, Timestamp::now().as_i64())
        .await
        .unwrap();
}

pub async fn hold_invoice_settlement(hash: &str) {
    let pool = crate::db::connect().await.unwrap();
    let order = crate::db::find_order_by_hash(&pool, hash).await.unwrap();
    info!(
        "Order Id: {} - Invoice with hash: {} was settled!",
        order.id, hash
    );
}

pub async fn hold_invoice_canceled(hash: &str) {
    let pool = crate::db::connect().await.unwrap();
    let order = crate::db::find_order_by_hash(&pool, hash).await.unwrap();
    info!(
        "Order Id: {} - Invoice with hash: {} was canceled!",
        order.id, hash
    );
}
