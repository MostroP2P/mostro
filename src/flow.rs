use log::info;
use nostr::key::FromBech32;
use nostr_sdk::Client;
use sqlx::SqlitePool;

pub async fn hold_invoice_paid(hash: &str, pool: &SqlitePool, client: &Client) {
    let order = crate::db::find_order_by_hash(pool, hash).await.unwrap();
    let my_keys = crate::util::get_keys().unwrap();
    let seller_pubkey = order.seller_pubkey.as_ref().unwrap();
    let seller_keys = nostr::key::Keys::from_bech32_public_key(seller_pubkey).unwrap();
    let buyer_pubkey = order.buyer_pubkey.as_ref().unwrap();
    let buyer_keys = nostr::key::Keys::from_bech32_public_key(buyer_pubkey).unwrap();

    info!(
        "Order Id: {} - Seller paid invoice with hash: {hash}",
        order.id
    );

    // We publish a new replaceable kind nostr event with the status updated
    // and update on local database the status and new event id
    crate::util::update_order_event(pool, client, &my_keys, crate::types::Status::Active, &order)
        .await
        .unwrap();

    // We send a confirmation message to seller
    let message = crate::messages::buyer_took_order(&order, buyer_pubkey);
    crate::util::send_dm(client, &my_keys, &seller_keys, message)
        .await
        .unwrap();
    // We send a message to buyer saying seller paid
    let message = crate::messages::get_in_touch_with_seller(&order, seller_pubkey);
    crate::util::send_dm(client, &my_keys, &buyer_keys, message)
        .await
        .unwrap();
}

pub async fn hold_invoice_settlement(hash: &str, pool: &SqlitePool, client: &Client) {
    let order = crate::db::find_order_by_hash(pool, hash).await.unwrap();
    let my_keys = crate::util::get_keys().unwrap();
    let seller_pubkey = order.seller_pubkey.as_ref().unwrap();
    let seller_keys = nostr::key::Keys::from_bech32_public_key(seller_pubkey).unwrap();
    let buyer_pubkey = order.buyer_pubkey.as_ref().unwrap();
    let buyer_keys = nostr::key::Keys::from_bech32_public_key(buyer_pubkey).unwrap();
    info!(
        "Order Id: {} - Seller released funds for invoice hash: {hash}",
        order.id
    );

    // We publish a new replaceable kind nostr event with the status updated
    // and update on local database the status and new event id
    crate::util::update_order_event(
        pool,
        client,
        &my_keys,
        crate::types::Status::SettledInvoice,
        &order,
    )
    .await
    .unwrap();
    // We send a *funds released* message to seller
    let message = crate::messages::sell_success(buyer_pubkey);
    crate::util::send_dm(client, &my_keys, &seller_keys, message)
        .await
        .unwrap();
    // We send a message to buyer saying seller released
    let message = crate::messages::funds_released(seller_pubkey);
    crate::util::send_dm(client, &my_keys, &buyer_keys, message)
        .await
        .unwrap();
}

pub async fn hold_invoice_canceled(hash: &str, pool: &SqlitePool, client: &Client) {
    let order = crate::db::find_order_by_hash(pool, hash).await.unwrap();
    let my_keys = crate::util::get_keys().unwrap();
    let seller_pubkey = order.seller_pubkey.as_ref().unwrap();
    let seller_keys = nostr::key::Keys::from_bech32_public_key(seller_pubkey).unwrap();
    let buyer_pubkey = order.buyer_pubkey.as_ref().unwrap();
    let buyer_keys = nostr::key::Keys::from_bech32_public_key(buyer_pubkey).unwrap();
    // If this invoice was Canceled
    info!(
        "Order Id: {} - Invoice with hash: {hash} was canceled!",
        order.id
    );
    // We publish a new replaceable kind nostr event with the status updated
    // and update on local database the status and new event id
    crate::util::update_order_event(
        pool,
        client,
        &my_keys,
        crate::types::Status::Canceled,
        &order,
    )
    .await
    .unwrap();
    // We send "order canceled" messages to both parties
    let message = crate::messages::order_canceled(order.id);
    crate::util::send_dm(client, &my_keys, &seller_keys, message.clone())
        .await
        .unwrap();
    crate::util::send_dm(client, &my_keys, &buyer_keys, message)
        .await
        .unwrap();
}
