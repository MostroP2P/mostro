use crate::util::{publish_order, send_dm};

use anyhow::Result;
use mostro_core::{Action, Message};
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
        let initiator_ephemeral_pubkey = event.pubkey.to_bech32()?;
        let master_pubkey = match msg.pubkey {
            Some(ref pk) => pk,
            None => {
                // We create a Message
                let message = Message::new(0, order.id, None, Action::CantDo, None);
                let message = message.as_json()?;
                send_dm(client, my_keys, &event.pubkey, message).await?;

                return Ok(());
            }
        };

        publish_order(
            pool,
            client,
            my_keys,
            order,
            &initiator_ephemeral_pubkey,
            master_pubkey,
        )
        .await?;
    }
    Ok(())
}
