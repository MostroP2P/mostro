use crate::lightning::LndConnector;
use crate::util::send_dm;

use anyhow::Result;
use log::error;
use mostro_core::order::Order;
use mostro_core::{Action, Message};
use nostr_sdk::prelude::*;
use sqlx::{Pool, Sqlite};
use sqlx_crud::Crud;

pub async fn admin_cancel_action(
    msg: Message,
    event: &Event,
    my_keys: &Keys,
    client: &Client,
    pool: &Pool<Sqlite>,
    ln_client: &mut LndConnector,
) -> Result<()> {
    let order_id = msg.order_id.unwrap();
    let order = match Order::by_id(pool, order_id).await? {
        Some(order) => order,
        None => {
            error!("AdminCancel: Order Id {order_id} not found!");
            return Ok(());
        }
    };
    // Check if the pubkey is Mostro
    if Some(event.pubkey.to_bech32()?) != order.buyer_pubkey {
        // We create a Message
        let message = Message::new(0, Some(order.id), None, Action::CantDo, None);
        let message = message.as_json()?;
        send_dm(client, my_keys, &event.pubkey, message).await?;

        return Ok(());
    }

    Ok(())
}
