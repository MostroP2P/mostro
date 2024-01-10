use crate::util::{send_dm, update_order_event};

use anyhow::Result;
use mostro_core::message::{Action, Content, Message, Peer};
use mostro_core::order::{Order, Status};
use nostr_sdk::prelude::*;
use sqlx::{Pool, Sqlite};
use sqlx_crud::Crud;
use std::str::FromStr;
use tracing::error;

pub async fn fiat_sent_action(
    msg: Message,
    event: &Event,
    my_keys: &Keys,
    client: &Client,
    pool: &Pool<Sqlite>,
) -> Result<()> {
    let order_id = msg.get_inner_message_kind().id.unwrap();
    let order = match Order::by_id(pool, order_id).await? {
        Some(order) => order,
        None => {
            error!("Order Id {order_id} not found!");
            return Ok(());
        }
    };
    // Send to user a DM with the error
    if order.status != "Active" {
        let error = format!("Order Id {order_id} wrong status");
        let message = Message::cant_do(Some(order.id), None, Some(Content::TextMessage(error)));
        send_dm(client, my_keys, &event.pubkey, message.as_json()?).await?;

        return Ok(());
    }
    // Check if the pubkey is the buyer
    if Some(event.pubkey.to_string()) != order.buyer_pubkey {
        let message = Message::cant_do(Some(order.id), None, None);
        send_dm(client, my_keys, &event.pubkey, message.as_json()?).await?;

        return Ok(());
    }

    // We publish a new replaceable kind nostr event with the status updated
    // and update on local database the status and new event id
    update_order_event(pool, client, my_keys, Status::FiatSent, &order).await?;

    let seller_pubkey = match order.seller_pubkey.as_ref() {
        Some(pk) => XOnlyPublicKey::from_str(pk)?,
        None => {
            error!("Seller pubkey not found for order {}!", order.id);
            return Ok(());
        }
    };
    let peer = Peer::new(event.pubkey.to_string());

    // We create a Message
    let message = Message::new_order(
        Some(order.id),
        None,
        Action::FiatSent,
        Some(Content::Peer(peer)),
    );
    let message = message.as_json().unwrap();
    send_dm(client, my_keys, &seller_pubkey, message).await?;
    // We send a message to buyer to wait
    let peer = Peer::new(seller_pubkey.to_string());

    // We create a Message
    let message = Message::new_order(
        Some(order.id),
        None,
        Action::FiatSent,
        Some(Content::Peer(peer)),
    );
    let message = message.as_json()?;
    send_dm(client, my_keys, &event.pubkey, message).await?;

    Ok(())
}
