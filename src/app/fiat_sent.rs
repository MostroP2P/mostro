use crate::util::{send_cant_do_msg, send_new_order_msg, update_order_event};

use anyhow::{Error, Result};
use mostro_core::message::{Action, Message, Payload, Peer};
use mostro_core::order::{Order, Status};
use nostr::nips::nip59::UnwrappedGift;
use nostr_sdk::prelude::*;
use sqlx::{Pool, Sqlite};
use sqlx_crud::Crud;
use std::str::FromStr;
use tracing::error;

pub async fn fiat_sent_action(
    msg: Message,
    event: &UnwrappedGift,
    my_keys: &Keys,
    pool: &Pool<Sqlite>,
) -> Result<()> {
    // Get request id
    let request_id = msg.get_inner_message_kind().request_id;

    let order_id = if let Some(order_id) = msg.get_inner_message_kind().id {
        order_id
    } else {
        return Err(Error::msg("No order id"));
    };
    let order = match Order::by_id(pool, order_id).await? {
        Some(order) => order,
        None => {
            error!("Order Id {order_id} not found!");
            return Ok(());
        }
    };
    // Send to user a DM with the error
    if order.status != Status::Active.to_string() {
        send_new_order_msg(
            request_id,
            Some(order.id),
            Action::NotAllowedByStatus,
            None,
            &event.rumor.pubkey,
            None,
        )
        .await;
        return Ok(());
    }
    // Check if the pubkey is the buyer
    if Some(event.rumor.pubkey.to_string()) != order.buyer_pubkey {
        send_cant_do_msg(request_id, Some(order.id), None, &event.rumor.pubkey).await;
        return Ok(());
    }

    // We publish a new replaceable kind nostr event with the status updated
    // and update on local database the status and new event id
    if let Ok(order_updated) = update_order_event(my_keys, Status::FiatSent, &order).await {
        let _ = order_updated.update(pool).await;
    }

    let seller_pubkey = match order.seller_pubkey.as_ref() {
        Some(pk) => PublicKey::from_str(pk)?,
        None => {
            error!("Seller pubkey not found for order {}!", order.id);
            return Ok(());
        }
    };
    let peer = Peer::new(event.rumor.pubkey.to_string());

    // We a message to the seller
    send_new_order_msg(
        None,
        Some(order.id),
        Action::FiatSentOk,
        Some(Payload::Peer(peer)),
        &seller_pubkey,
        None,
    )
    .await;
    // We send a message to buyer to wait
    let peer = Peer::new(seller_pubkey.to_string());

    send_new_order_msg(
        msg.get_inner_message_kind().request_id,
        Some(order.id),
        Action::FiatSentOk,
        Some(Payload::Peer(peer)),
        &event.rumor.pubkey,
        None,
    )
    .await;

    Ok(())
}
