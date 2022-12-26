use crate::types::{self, Order};
use anyhow::Result;
use log::info;
use nostr::util::time::timestamp;
use nostr::{EventBuilder, Kind};
use nostr_sdk::nostr::Keys;
use nostr_sdk::Client;
use sqlx::SqlitePool;

pub async fn publish_order(
    pool: &SqlitePool,
    client: &Client,
    keys: &Keys,
    order: &Order,
    initiator_pubkey: &str,
) -> Result<()> {
    let order = Order::new(
        order.kind,
        types::Status::Pending,
        order.amount,
        order.fiat_code.to_owned(),
        order.fiat_amount,
        order.payment_method.to_owned(),
        order.prime,
        None,
        Some(timestamp()),
    );
    let order_string = order.as_json().unwrap();
    let event = EventBuilder::new(Kind::Custom(11000), &order_string, &[])
        .to_event(keys)
        .unwrap();
    let event_id = event.id.to_string();

    info!("Event published: {:#?}", event);
    let order_id = crate::db::add_order(pool, &order, &event_id, initiator_pubkey).await?;
    info!("New order saved Id: {order_id}");

    client
        .send_event(event)
        .await
        .map(|_s| ())
        .map_err(|err| err.into())
}

pub async fn send_dm(
    client: &Client,
    sender_keys: &Keys,
    receiver_keys: &Keys,
    content: String,
) -> Result<()> {
    let event = EventBuilder::new_encrypted_direct_msg(sender_keys, receiver_keys, content)?
        .to_event(&sender_keys)?;
    info!("Sending event: {event:#?}");
    client.send_event(event).await?;

    Ok(())
}

pub fn get_keys() -> Result<nostr::Keys> {
    use std::env;
    // From Bech32
    use nostr::key::FromBech32;
    // nostr private key
    let nsec1privkey = env::var("NSEC_PRIVKEY").expect("$NSEC_PRIVKEY is not set");

    Ok(Keys::from_bech32(&nsec1privkey)?)
}
