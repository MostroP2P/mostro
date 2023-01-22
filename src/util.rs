use crate::types::{self, Order};
use anyhow::Result;
use dotenvy::var;
use log::{error, info};
use nostr::event::tag::{Tag, TagKind};
use nostr::key::FromSkStr;
use nostr::util::time::timestamp;
use nostr::{EventBuilder, Kind};
use nostr_sdk::nostr::Keys;
use nostr_sdk::Client;
use sqlx::SqlitePool;
use std::str::FromStr;

pub async fn publish_order(
    pool: &SqlitePool,
    client: &Client,
    keys: &Keys,
    order: &Order,
    initiator_pubkey: &str,
) -> Result<()> {
    let order_id = crate::db::add_order(pool, order, "", initiator_pubkey).await?;
    info!("New order saved Id: {order_id}");
    // Now we have the order id, we can create a new event adding this id to the Order object
    let order = Order::new(
        Some(order_id),
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
    // This tag (nip33) allows us to change this event in particular in the future
    let event_kind = 30000;
    let d_tag = Tag::Generic(TagKind::Custom("d".to_string()), vec![order_id.to_string()]);
    let event = EventBuilder::new(Kind::Custom(event_kind as u64), &order_string, &[d_tag])
        .to_event(keys)
        .unwrap();
    let event_id = event.id.to_string();
    info!("Publishing Event Id: {event_id} for Order Id: {order_id}");
    // We update the order id with the new event_id
    crate::db::update_order_event_id_status(pool, order_id, &types::Status::Pending, &event_id)
        .await?;
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
    // nostr private key
    let nsec1privkey = var("NSEC_PRIVKEY").expect("$NSEC_PRIVKEY is not set");
    let my_keys = nostr::key::Keys::from_sk_str(&nsec1privkey)?;

    Ok(my_keys)
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
        Some(order.id),
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
    // nip33 kind and d tag
    let event_kind = 30000;
    let d_tag = Tag::Generic(TagKind::Custom("d".to_string()), vec![order.id.to_string()]);
    let event = EventBuilder::new(Kind::Custom(event_kind), &order_string, &[d_tag])
        .to_event(keys)
        .unwrap();
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

pub async fn connect_nostr() -> Result<nostr_sdk::Client> {
    let my_keys = crate::util::get_keys()?;

    // Create new client
    let client = nostr_sdk::Client::new(&my_keys);

    let relays = vec![
        "wss://nostr.p2sh.co",
        "wss://relay.nostr.vision",
        "wss://nostr.itssilvestre.com",
        "wss://nostr.drss.io",
        "wss://relay.nostr.info",
        "wss://relay.nostr.pro",
        "wss://nostr.zebedee.cloud",
        "wss://nostr.fmt.wiz.biz",
        "wss://public.nostr.swissrouting.com",
        "wss://nostr.slothy.win",
        "wss://nostr.rewardsbunny.com",
        "wss://relay.nostropolis.xyz/websocket",
        "wss://nostr.supremestack.xyz",
        "wss://nostr.orba.ca",
        "wss://nostr.mom",
        "wss://nostr.yael.at",
        "wss://nostr-relay.derekross.me",
        "wss://nostr.shawnyeager.net",
        "wss://nostr.jiashanlu.synology.me",
        "wss://relay.ryzizub.com",
        "wss://relay.nostrmoto.xyz",
        "wss://jiggytom.ddns.net",
        "wss://nostr.sectiontwo.org",
        "wss://nostr.roundrockbitcoiners.com",
        "wss://nostr.utxo.lol",
        "wss://relay.nostrid.com",
        "wss://nostr1.starbackr.me",
        "wss://nostr-relay.schnitzel.world",
        "wss://sg.qemura.xyz",
        "wss://nostr.digitalreformation.info",
        "wss://nostr-relay.usebitcoin.space",
        "wss://nostr.bch.ninja",
        "wss://nostr.demovement.net",
        "wss://nostr.massmux.com",
        "wss://relay.nostr.bg",
        "wss://nostr-pub1.southflorida.ninja",
        "wss://nostr.itssilvestre.com",
        "wss://nostr-1.nbo.angani.co",
        "wss://relay.nostr.nu",
        "wss://nostr.easydns.ca",
        "wss://no-str.org",
        "wss://nostr.milou.lol",
        "wss://nostrical.com",
        "wss://pow32.nostr.land",
        "wss://student.chadpolytechnic.com",
    ];

    // Add relays
    for r in relays {
        client.add_relay(r, None).await?;
    }

    // Connect to relays and keep connection alive
    client.connect().await?;

    Ok(client)
}
