use crate::db::{update_order_buyer_dispute, update_order_seller_dispute};
use crate::util::send_dm;

use anyhow::Result;
use log::error;
use mostro_core::order::Order;
use mostro_core::{Action, Message};
use nostr_sdk::prelude::*;
use sqlx::{Pool, Sqlite};
use sqlx_crud::Crud;

pub async fn dispute_action(
    msg: Message,
    event: &Event,
    my_keys: &Keys,
    client: &Client,
    pool: &Pool<Sqlite>,
) -> Result<()> {
    let order_id = msg.order_id.unwrap();
    let order = match Order::by_id(pool, order_id).await? {
        Some(order) => order,
        None => {
            error!("Dispute: Order Id {order_id} not found!");
            return Ok(());
        }
    };

    let buyer = order.buyer_pubkey.unwrap();
    let seller = order.seller_pubkey.unwrap();
    let message_sender = event.pubkey.to_bech32()?;
    // Get counterpart pubkey
    let mut counterpart: String = String::new();
    let mut buyer_dispute: bool = false;
    let mut seller_dispute: bool = false;

    // Find the counterpart public key
    if message_sender == buyer {
        counterpart = seller;
        buyer_dispute = true;
    } else if message_sender == seller {
        counterpart = buyer;
        seller_dispute = true;
    };

    // Add a check in case of no counterpart found
    if counterpart.is_empty() {
        // We create a Message
        let message = Message::new(0, Some(order.id), None, Action::CantDo, None);
        let message = message.as_json()?;
        send_dm(client, my_keys, &event.pubkey, message).await?;

        return Ok(());
    };

    let mut update_seller_dispute = false;
    let mut update_buyer_dispute = false;
    if seller_dispute && !order.seller_dispute {
        update_seller_dispute = true;
        update_order_seller_dispute(pool, order_id, update_seller_dispute).await?;
    } else if buyer_dispute && !order.buyer_dispute {
        update_buyer_dispute = true;
        update_order_buyer_dispute(pool, order_id, update_buyer_dispute).await?;
    };
    if !update_buyer_dispute && !update_seller_dispute {
        return Ok(());
    };

    Ok(())
}
