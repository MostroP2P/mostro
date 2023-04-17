use crate::util::publish_order;

use anyhow::Result;
use mostro_core::Message;
use nostr_sdk::prelude::ToBech32;
use nostr_sdk::{Client, Event, Keys};
use sqlx::{Pool, Sqlite};

pub async fn order_action(
    msg: Message,
    event: &Event,
    my_keys: &Keys,
    client: &Client,
    pool: &Pool<Sqlite>,
) -> Result<()> {
    if let Some(order) = msg.get_order() {
        let initiator_pubkey = event.pubkey.to_bech32()?;

        publish_order(pool, client, my_keys, order, &initiator_pubkey).await?;
    }
    Ok(())
}
