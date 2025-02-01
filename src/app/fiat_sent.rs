use crate::util::{enqueue_order_msg, get_order, update_order_event};

use mostro_core::error::{
    CantDoReason,
    MostroError::{self, *},
    ServiceError,
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

    // Get next trade key
    let next_trade = match msg.get_inner_message_kind().get_next_trade_key() {
        Ok((pubkey, index)) => (pubkey, index),
        Err(cause) => return Err(MostroInternalErr(cause)),
    };

    // We publish a new replaceable kind nostr event with the status updated
    // and update on local database the status and new event id
    let order_updated = update_order_event(my_keys, Status::FiatSent, &order)
        .await
        .map_err(|e| MostroError::MostroInternalErr(ServiceError::NostrError(e.to_string())))?;

    // Update order
    let mut order_updated = order_updated
        .update(pool)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

    let seller_pubkey = order.get_seller_pubkey().map_err(MostroInternalErr)?;

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
    match order.sent_from_maker(event.rumor.pubkey) {
        Ok(_) => {
            order_updated.next_trade_pubkey = Some(next_trade.0);
            order_updated.next_trade_index = Some(next_trade.1 as i64);
            if let Err(e) = order_updated.update(pool).await {
                return Err(MostroInternalErr(ServiceError::DbAccessError(
                    e.to_string(),
                )));
            }
        }
        Err(cause) => return Err(MostroCantDo(cause)),
    }

    Ok(())
}
