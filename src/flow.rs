use crate::util::send_dm;
use log::info;
use mostro_core::{order::SmallOrder, Action, Content, Message, Status};
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

    // We publish a new replaceable kind nostr event with the status updated
    // and update on local database the status and new event id
    crate::util::update_order_event(&pool, &client, &my_keys, Status::Active, &order)
        .await
        .unwrap();
    // We send this data related to the order to the parties
    let order_data = SmallOrder::new(
        order.id,
        order.amount,
        order.fiat_code,
        order.fiat_amount,
        order.payment_method,
        order.premium,
        order.buyer_pubkey,
        order.seller_pubkey,
    );
    // We send a confirmation message to seller
    let message = Message::new(
        0,
        Some(order.id),
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
        Action::HoldInvoicePaymentAccepted,
        Some(Content::SmallOrder(order_data)),
    );
    let message = message.as_json().unwrap();
    send_dm(&client, &my_keys, &buyer_pubkey, message)
        .await
        .unwrap();
}

pub async fn hold_invoice_settlement(hash: &str) {
    let pool = crate::db::connect().await.unwrap();
    let client = crate::util::connect_nostr().await.unwrap();
    let order = crate::db::find_order_by_hash(&pool, hash).await.unwrap();
    let my_keys = crate::util::get_keys().unwrap();
    let seller_pubkey = XOnlyPublicKey::from_bech32(order.seller_pubkey.as_ref().unwrap()).unwrap();
    let buyer_pubkey = XOnlyPublicKey::from_bech32(order.buyer_pubkey.as_ref().unwrap()).unwrap();
    info!(
        "Order Id: {} - Seller released funds for invoice hash: {hash}",
        order.id
    );

    // We publish a new replaceable kind nostr event with the status updated
    // and update on local database the status and new event id
    crate::util::update_order_event(&pool, &client, &my_keys, Status::SettledHoldInvoice, &order)
        .await
        .unwrap();
    // We send a *funds released* message to seller
    let text_message = crate::messages::sell_success(order.id, buyer_pubkey).unwrap();
    // We create a Message
    let message = Message::new(
        0,
        Some(order.id),
        Action::HoldInvoicePaymentSettled,
        Some(Content::TextMessage(text_message)),
    );
    let message = message.as_json().unwrap();
    send_dm(&client, &my_keys, &seller_pubkey, message)
        .await
        .unwrap();
    // We send a message to buyer saying seller released
    let text_message = crate::messages::funds_released(order.id, seller_pubkey).unwrap();
    // We create a Message
    let message = Message::new(
        0,
        Some(order.id),
        Action::Release,
        Some(Content::TextMessage(text_message)),
    );
    let message = message.as_json().unwrap();
    send_dm(&client, &my_keys, &buyer_pubkey, message)
        .await
        .unwrap();
}

pub async fn hold_invoice_canceled(hash: &str) {
    let pool = crate::db::connect().await.unwrap();
    let client = crate::util::connect_nostr().await.unwrap();
    let order = crate::db::find_order_by_hash(&pool, hash).await.unwrap();
    let my_keys = crate::util::get_keys().unwrap();
    let seller_pubkey = order.seller_pubkey.as_ref().unwrap();
    let seller_pubkey = XOnlyPublicKey::from_bech32(seller_pubkey).unwrap();
    let buyer_pubkey = order.buyer_pubkey.as_ref().unwrap();
    let buyer_pubkey = XOnlyPublicKey::from_bech32(buyer_pubkey).unwrap();
    // If this invoice was Canceled
    info!(
        "Order Id: {} - Invoice with hash: {hash} was canceled!",
        order.id
    );
    // We publish a new replaceable kind nostr event with the status updated
    // and update on local database the status and new event id
    crate::util::update_order_event(&pool, &client, &my_keys, Status::Canceled, &order)
        .await
        .unwrap();
    // We send "order canceled" messages to both parties
    let text_message = crate::messages::order_canceled(order.id);
    // We create a Message
    let message = Message::new(
        0,
        Some(order.id),
        Action::HoldInvoicePaymentCanceled,
        Some(Content::TextMessage(text_message)),
    );
    let message = message.as_json().unwrap();
    send_dm(&client, &my_keys, &seller_pubkey, message.clone())
        .await
        .unwrap();
    send_dm(&client, &my_keys, &buyer_pubkey, message)
        .await
        .unwrap();
}
