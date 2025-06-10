use crate::util::{enqueue_order_msg, get_order, update_order_event};
use mostro_core::prelude::*;
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

    // Check if the pubkey is the buyer pubkey - Only the buyer can send fiat
    // if someone else tries to send fiat, we return an error
    if order.get_buyer_pubkey().ok() != Some(event.rumor.pubkey) {
        return Err(MostroCantDo(CantDoReason::InvalidPubkey));
    }

    // Get next trade key
    let next_trade = msg
        .get_inner_message_kind()
        .get_next_trade_key()
        .map_err(MostroInternalErr)?;

    // We publish a new replaceable kind nostr event with the status updated
    // and update on local database the status and new event id
    let mut order_updated = update_order_event(my_keys, Status::FiatSent, &order)
        .await
        .map_err(|e| MostroError::MostroInternalErr(ServiceError::NostrError(e.to_string())))?;

    let seller_pubkey = order.get_seller_pubkey().map_err(MostroInternalErr)?;

    // Create peer
    let peer = Peer {
        pubkey: event.rumor.pubkey.to_string(),
        reputation: None,
    };

    // Notify seller that fiat was sent
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
    let peer = Peer {
        pubkey: seller_pubkey.to_string(),
        reputation: None,
    };

    // Notify buyer that fiat was sent
    enqueue_order_msg(
        msg.get_inner_message_kind().request_id,
        Some(order_updated.id),
        Action::FiatSentOk,
        Some(Payload::Peer(peer)),
        event.rumor.pubkey,
        None,
    )
    .await;

    // If this is a range order, we need to update next trade fields
    if order.is_range_order() {
        // Update next trade fields only when the buyer is the maker of a range order
        // These fields will be used to create the next child order in the range
        if let Some((pubkey, index)) = next_trade {
            order_updated.next_trade_pubkey = Some(pubkey);
            order_updated.next_trade_index = Some(index as i64);
        }
    }

    // Update order
    order_updated
        .update(pool)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

    Ok(())
}
