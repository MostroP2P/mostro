use crate::util::send_dm;
use mostro_core::message::{Action, Content, Message};
use mostro_core::order::{Kind, SmallOrder, Status};
use nostr_sdk::prelude::*;
use std::str::FromStr;
use tracing::{error, info};

pub async fn hold_invoice_paid(hash: &str) {
    let pool = crate::db::connect().await.unwrap();
    let client = crate::util::connect_nostr().await.unwrap();
    let order = crate::db::find_order_by_hash(&pool, hash).await.unwrap();
    let my_keys = crate::util::get_keys().unwrap();
    let seller_pubkey = match XOnlyPublicKey::from_str(order.seller_pubkey.as_ref().unwrap()) {
        Ok(pk) => pk,
        Err(e) => {
            error!("Order Id {} wrong seller pubkey: {:?}", order.id, e);
            return;
        }
    };
    let buyer_pubkey = match XOnlyPublicKey::from_str(order.buyer_pubkey.as_ref().unwrap()) {
        Ok(pk) => pk,
        Err(e) => {
            error!("Order Id {} wrong buyer pubkey: {:?}", order.id, e);
            return;
        }
    };

    info!(
        "Order Id: {} - Seller paid invoice with hash: {hash}",
        order.id
    );

    let order_kind = match Kind::from_str(&order.kind) {
        Ok(k) => k,
        Err(e) => {
            error!("Order Id {} wrong kind: {:?}", order.id, e);
            return;
        }
    };

    // We send this data related to the order to the parties
    let mut order_data = SmallOrder::new(
        Some(order.id),
        Some(order_kind),
        None,
        order.amount,
        order.fiat_code.clone(),
        order.fiat_amount,
        order.payment_method.clone(),
        order.premium,
        order.buyer_pubkey.as_ref().cloned(),
        order.seller_pubkey.as_ref().cloned(),
        None,
        Some(order.created_at),
    );
    let status;

    if order.buyer_invoice.is_some() {
        status = Status::Active;
        order_data.status = Some(status);
        // We send a confirmation message to seller
        let message = Message::new_order(
            Some(order.id),
            None,
            Action::BuyerTookOrder,
            Some(Content::Order(order_data.clone())),
        );
        let message = message.as_json().unwrap();
        send_dm(&client, &my_keys, &seller_pubkey, message)
            .await
            .unwrap();
        // We send a message to buyer saying seller paid
        let message = Message::new_order(
            Some(order.id),
            None,
            Action::HoldInvoicePaymentAccepted,
            Some(Content::Order(order_data)),
        );
        let message = message.as_json().unwrap();
        send_dm(&client, &my_keys, &buyer_pubkey, message)
            .await
            .unwrap();
    } else {
        let new_amount = order_data.amount - order.fee;
        order_data.amount = new_amount;
        status = Status::WaitingBuyerInvoice;
        order_data.status = Some(status);
        order_data.master_buyer_pubkey = None;
        order_data.master_seller_pubkey = None;
        // We ask to buyer for a new invoice
        let message = Message::new_order(
            Some(order.id),
            None,
            Action::AddInvoice,
            Some(Content::Order(order_data)),
        );
        let message = message.as_json().unwrap();
        send_dm(&client, &my_keys, &buyer_pubkey, message)
            .await
            .unwrap();
        // We send a message to seller we are waiting for buyer invoice
        let message = Message::new_order(Some(order.id), None, Action::WaitingBuyerInvoice, None);
        let message = message.as_json().unwrap();
        send_dm(&client, &my_keys, &seller_pubkey, message)
            .await
            .unwrap();
    }
    // We publish a new replaceable kind nostr event with the status updated
    // and update on local database the status and new event id
    crate::util::update_order_event(&pool, &client, &my_keys, status, &order)
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
