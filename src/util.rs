use crate::types::{self, Order};
use anyhow::Result;
use log::info;
use nostr::util::time::timestamp;
use nostr::{EventBuilder, Kind};
use nostr_sdk::nostr::Keys;
use nostr_sdk::Client;

pub async fn publish_order(client: &Client, keys: &Keys, order: &Order) -> Result<()> {
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
    let order = order.as_json().unwrap();
    let event = EventBuilder::new(Kind::Custom(11000), &order, &[])
        .to_event(keys)
        .unwrap();

    info!("Event published: {:#?}", event);

    client.send_event(event).await
}
