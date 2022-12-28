use crate::types::{self, Order};
use anyhow::Result;
use log::info;
use nostr::util::time::timestamp;
use nostr::{Event, EventBuilder, Kind};
use nostr_sdk::nostr::Keys;
use nostr_sdk::Client;
use sqlx::SqlitePool;
use sqlx_crud::Crud;
use std::str::FromStr;

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
        .to_event(sender_keys)?;
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

pub fn get_event_id_from_dm(event: &Event) -> Result<String> {
    let id = event
        .tags
        .iter()
        .find(|t| matches!(t.kind(), Ok(nostr::event::tag::TagKind::E)));

    let id = id
        .expect("This message is not related to another event")
        .content()
        .expect("No event related");

    Ok(id.to_string())
}

pub async fn update_order_event(
    pool: &SqlitePool,
    client: &Client,
    keys: &Keys,
    status: types::Status,
    order: &crate::models::Order,
) -> Result<()> {
    let kind = crate::types::Kind::from_str(&order.kind).unwrap();
    let publish_order = Order::new(
        kind,
        status.clone(),
        order.amount as u32,
        order.fiat_code.to_owned(),
        order.fiat_amount as u32,
        order.payment_method.to_owned(),
        0,
        None,
        Some(timestamp()),
    );
    let order_string = publish_order.as_json().unwrap();
    let event = EventBuilder::new(Kind::Custom(11000), &order_string, &[])
        .to_event(keys)
        .unwrap();
    let event_id = event.id.to_string();

    info!(
        "Order Id: {} updated Nostr Event to Id {}, new Status: {}",
        order.id,
        event_id,
        status.to_string()
    );
    let mut order = crate::db::find_order_by_event_id(pool, &order.event_id).await?;
    order.status = status.to_string();
    order.event_id = event_id;
    order.update(pool).await?;

    client
        .send_event(event)
        .await
        .map(|_s| ())
        .map_err(|err| err.into())
}

pub async fn connect_nostr() -> Result<nostr_sdk::Client> {
    let my_keys = crate::util::get_keys()?;

    // Create new client
    let client = nostr_sdk::Client::new(&my_keys);

    // Add relays
    // client.add_relay("wss://relay.grunch.dev", None).await?;
    client.add_relay("wss://nostr.fly.dev", None).await?;
    client
        .add_relay("wss://relay.cryptocculture.com", None)
        .await?;
    // client.add_relay("wss://relay.damus.io", None).await?;
    // client.add_relay("wss://nostr.openchain.fr", None).await?;

    // Connect to relays and keep connection alive
    client.connect().await?;

    Ok(client)
}
