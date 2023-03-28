use log::info;
use mostro_core::Status;
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

    // We send a confirmation message to seller
    let message = crate::messages::buyer_took_order(&order, buyer_pubkey).unwrap();
    crate::util::send_dm(&client, &my_keys, &seller_pubkey, message)
        .await
        .unwrap();
    // We send a message to buyer saying seller paid
    let message = crate::messages::get_in_touch_with_seller(&order, seller_pubkey).unwrap();
    crate::util::send_dm(&client, &my_keys, &buyer_pubkey, message)
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
    let message = crate::messages::sell_success(order.id, buyer_pubkey).unwrap();
    crate::util::send_dm(&client, &my_keys, &seller_pubkey, message)
        .await
        .unwrap();
    // We send a message to buyer saying seller released
    let message = crate::messages::funds_released(order.id, seller_pubkey).unwrap();
    crate::util::send_dm(&client, &my_keys, &buyer_pubkey, message)
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
    let message = crate::messages::order_canceled(order.id);
    crate::util::send_dm(&client, &my_keys, &seller_pubkey, message.clone())
        .await
        .unwrap();
    crate::util::send_dm(&client, &my_keys, &buyer_pubkey, message)
        .await
        .unwrap();
}
