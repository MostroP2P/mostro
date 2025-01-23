use crate::util::{enqueue_order_msg, get_order, update_order_event};

use anyhow::Result;
use mostro_core::error::{
    CantDoReason,
    MostroError::{self, *},
};
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
    if order.get_buyer_pubkey().ok() != Some(event.rumor.pubkey) {
        return Err(MostroCantDo(CantDoReason::InvalidPubkey));
    }
    let next_trade: Option<(String, u32)> = match &msg.get_inner_message_kind().payload {
        Some(Payload::NextTrade(pubkey, index)) => Some((pubkey.clone(), *index)),
        _ => None,
    };
    // We publish a new replaceable kind nostr event with the status updated
    // and update on local database the status and new event id
    if let Ok(order_updated) = update_order_event(my_keys, Status::FiatSent, &order).await {
        let _ = order_updated.update(pool).await;
    }

    let seller_pubkey = order
        .get_seller_pubkey()
        .map_err(|cause| MostroInternalErr(cause))?;

    // Create peer
    let peer = Peer::new(event.rumor.pubkey.to_string());

    // We a message to the seller
    enqueue_order_msg(
        None,
        Some(order_updated.id),
        Action::FiatSentOk,
        Some(Payload::Peer(peer)),
        seller_pubkey,
        None,
    )
    .await;
    // We send a message to buyer to wait
    let peer = Peer::new(seller_pubkey.to_string());

    enqueue_order_msg(
        msg.get_inner_message_kind().request_id,
        Some(order_updated.id),
        Action::FiatSentOk,
        Some(Payload::Peer(peer)),
        event.rumor.pubkey,
        None,
    )
    .await;

    // Update next trade fields only when the buyer is the maker of a range order
    // These fields will be used to create the next child order in the range
    if order_updated.creator_pubkey == event.rumor.pubkey.to_string() && next_trade.is_some() {
        if let Some((pubkey, index)) = next_trade {
            order_updated.next_trade_pubkey = Some(pubkey.clone());
            order_updated.next_trade_index = Some(index as i64);
            if let Err(e) = order_updated.update(pool).await {
                error!(
                    "Failed to update next trade fields for order {}: {}",
                    order_id, e
                );
                return Ok(());
            }
        }
    }

    Ok(())
}
