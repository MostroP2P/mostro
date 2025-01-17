use crate::util::{get_order, send_new_order_msg, update_order_event};

use anyhow::Result;
use mostro_core::error::{CantDoReason, MostroError, MostroError::*, ServiceError};
use mostro_core::message::{Action, Message, Payload, Peer};
use mostro_core::order::Status;
use nostr::nips::nip59::UnwrappedGift;
use nostr_sdk::prelude::*;
use sqlx::{Pool, Sqlite};
use sqlx_crud::Crud;

// Handle fiat sent action
pub async fn fiat_sent_action(
    msg: Message,
    event: &UnwrappedGift,
    my_keys: &Keys,
    pool: &Pool<Sqlite>,
) -> Result<(), MostroError> {
    // Get order
    let order = get_order(&msg, pool).await?;

    // Check if the order status is active
    if let Err(cause) = order.check_status(Status::Active) {
        return Err(MostroCantDo(cause));
    }

    // Check if the pubkey is the buyer
    if order.get_buyer_pubkey() != Some(event.rumor.pubkey) {
        return Err(MostroCantDo(CantDoReason::InvalidPubkey));
    }

    // We publish a new replaceable kind nostr event with the status updated
    // and update on local database the status and new event id
    if let Ok(order_updated) = update_order_event(my_keys, Status::FiatSent, &order).await {
        let _ = order_updated.update(pool).await;
    }

    let seller_pubkey = match order.get_seller_pubkey() {
        Some(pk) => pk,
        None => return Err(MostroInternalErr(ServiceError::InvalidPubkey)),
    };

    // Create peer
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
