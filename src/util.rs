use crate::models::{NewOrder, Order};
use crate::types::{Kind as OrderKind, Status};
use anyhow::Result;
use dotenvy::var;
use log::{error, info};
use nostr_sdk::prelude::*;
use sqlx::SqlitePool;
use std::str::FromStr;

pub async fn publish_order(
    pool: &SqlitePool,
    client: &Client,
    keys: &Keys,
    new_order: &NewOrder,
    initiator_pubkey: &str,
) -> Result<()> {
    let order = crate::db::add_order(pool, new_order, "", initiator_pubkey).await?;
    let order_id = order.id;
    info!("New order saved Id: {}", order_id);
    // Now we have the order id, we can create a new event adding this id to the Order object
    let order = NewOrder::new(
        Some(order_id),
        OrderKind::from_str(&order.kind).unwrap(),
        Status::Pending,
        order.amount,
        order.fiat_code,
        order.fiat_amount,
        order.payment_method,
        order.prime,
        None,
        Some(order.created_at),
    );

    let order_string = order.as_json().unwrap();
    info!("serialized order: {order_string}");
    // This tag (nip33) allows us to change this event in particular in the future
    let event_kind = 30000;
    let d_tag = Tag::Generic(TagKind::Custom("d".to_string()), vec![order_id.to_string()]);
    let event = EventBuilder::new(Kind::Custom(event_kind as u64), &order_string, &[d_tag])
        .to_event(keys)
        .unwrap();
    let event_id = event.id.to_string();
    info!("Publishing Event Id: {event_id} for Order Id: {order_id}");
    // We update the order id with the new event_id
    crate::db::update_order_event_id_status(pool, order_id, &Status::Pending, &event_id).await?;
    client
        .send_event(event)
        .await
        .map(|_s| ())
        .map_err(|err| err.into())
}

pub async fn send_dm(
    client: &Client,
    sender_keys: &Keys,
    receiver_pubkey: &XOnlyPublicKey,
    content: String,
) -> Result<()> {
    let event = EventBuilder::new_encrypted_direct_msg(sender_keys, *receiver_pubkey, content)?
        .to_event(sender_keys)?;
    info!("Sending event: {event:#?}");
    client.send_event(event).await?;

    Ok(())
}

pub fn get_keys() -> Result<Keys> {
    // nostr private key
    let nsec1privkey = var("NSEC_PRIVKEY").expect("NSEC_PRIVKEY is not set");
    let my_keys = Keys::from_sk_str(&nsec1privkey)?;

    Ok(my_keys)
}

pub async fn update_order_event(
    pool: &SqlitePool,
    client: &Client,
    keys: &Keys,
    status: Status,
    order: &Order,
) -> Result<()> {
    let kind = OrderKind::from_str(&order.kind).unwrap();
    let publish_order = NewOrder::new(
        Some(order.id),
        kind,
        status.clone(),
        order.amount,
        order.fiat_code.to_owned(),
        order.fiat_amount,
        order.payment_method.to_owned(),
        0,
        None,
        Some(order.created_at),
    );
    let order_string = publish_order.as_json()?;
    // nip33 kind and d tag
    let event_kind = 30000;
    let d_tag = Tag::Generic(TagKind::Custom("d".to_string()), vec![order.id.to_string()]);
    let event =
        EventBuilder::new(Kind::Custom(event_kind), &order_string, &[d_tag]).to_event(keys)?;
    let event_id = event.id.to_string();
    let status_str = status.to_string();
    info!("Sending replaceable event: {event:#?}");
    // We update the order id with the new event_id
    crate::db::update_order_event_id_status(pool, order.id, &status, &event_id).await?;
    info!(
        "Order Id: {} updated Nostr new Status: {}",
        order.id, status_str
    );

    client.send_event(event).await.map(|_s| ()).map_err(|err| {
        error!("{}", err);
        err.into()
    })
}

pub async fn connect_nostr() -> Result<Client> {
    let my_keys = crate::util::get_keys()?;

    // Create new client
    let client = Client::new(&my_keys);

    let relays = vec![
        "wss://relay.nostr.vision",
        "wss://nostr.zebedee.cloud",
        "wss://public.nostr.swissrouting.com",
        "wss://nostr.slothy.win",
        "wss://nostr.rewardsbunny.com",
        "wss://nostr.supremestack.xyz",
        "wss://nostr.shawnyeager.net",
        "wss://relay.nostrmoto.xyz",
        "wss://nostr.roundrockbitcoiners.com",
        "wss://nostr.utxo.lol",
        "wss://nostr-relay.schnitzel.world",
        "wss://sg.qemura.xyz",
        "wss://nostr.digitalreformation.info",
        "wss://nostr-relay.usebitcoin.space",
        "wss://nostr.bch.ninja",
        "wss://nostr.massmux.com",
        "wss://nostr-pub1.southflorida.ninja",
        "wss://relay.nostr.nu",
        "wss://nostr.easydns.ca",
        "wss://nostrical.com",
        "wss://relay.damus.io",
    ];

    // Add relays
    for r in relays.into_iter() {
        client.add_relay(r, None).await?;
    }

    // Connect to relays and keep connection alive
    client.connect().await;

    Ok(client)
}
