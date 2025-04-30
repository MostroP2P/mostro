use crate::util::{enqueue_order_msg, get_order, update_user_rating_event};

use crate::db::{is_user_present, update_user_rating};
use mostro_core::error::MostroError::{self, *};
use mostro_core::error::{CantDoReason, ServiceError};
use mostro_core::message::{Action, Message, Payload};
use mostro_core::order::{Order, Status};
use mostro_core::rating::Rating;
use nostr::nips::nip59::UnwrappedGift;
use nostr_sdk::prelude::*;
use sqlx::{Pool, Sqlite};

pub fn prepare_variables_for_vote(
    message_sender: &str,
    order: &Order,
) -> Result<(String, String, bool, bool), MostroError> {
    let mut counterpart: String = String::new();
    let mut counterpart_trade_pubkey: String = String::new();
    let mut buyer_rating: bool = false;
    let mut seller_rating: bool = false;

    // Get needed info about users
    let (seller, buyer) = match (&order.seller_pubkey, &order.buyer_pubkey) {
        (Some(seller), Some(buyer)) => (seller.to_owned(), buyer.to_owned()),
        (None, _) => return Err(MostroInternalErr(ServiceError::InvalidPubkey)),
        (_, None) => return Err(MostroInternalErr(ServiceError::InvalidPubkey)),
    };

    // Find the counterpart public key
    if message_sender == buyer {
        counterpart = order
            .get_master_seller_pubkey()
            .map_err(MostroInternalErr)?
            .to_string();
        buyer_rating = true;
        counterpart_trade_pubkey = order
            .get_buyer_pubkey()
            .map_err(MostroInternalErr)?
            .to_string();
    } else if message_sender == seller {
        counterpart = order
            .get_master_buyer_pubkey()
            .map_err(MostroInternalErr)?
            .to_string();
        seller_rating = true;
        counterpart_trade_pubkey = order
            .get_seller_pubkey()
            .map_err(MostroInternalErr)?
            .to_string();
    };
    // Add a check in case of no counterpart found
    if counterpart.is_empty() {
        return Err(MostroCantDo(CantDoReason::InvalidPeer));
    };
    Ok((
        counterpart,
        counterpart_trade_pubkey,
        buyer_rating,
        seller_rating,
    ))
}

pub async fn update_user_reputation_action(
    msg: Message,
    event: &UnwrappedGift,
    my_keys: &Keys,
    pool: &Pool<Sqlite>,
) -> Result<(), MostroError> {
    // Get order
    let order = get_order(&msg, pool).await?;

    // Check if order is success
    if order.check_status(Status::Success).is_err() {
        return Err(MostroCantDo(CantDoReason::InvalidOrderStatus));
    }

    // Prepare variables for vote
    let (counterpart, counterpart_trade_pubkey, buyer_rating, seller_rating) =
        prepare_variables_for_vote(&event.rumor.pubkey.to_string(), &order)?;

    // Check if the order is not rated by the message sender
    // Check what rate status needs update
    let mut update_seller_rate = false;
    let mut update_buyer_rate = false;
    if seller_rating && !order.seller_sent_rate {
        update_seller_rate = true;
    } else if buyer_rating && !order.buyer_sent_rate {
        update_buyer_rate = true;
    };
    if !update_buyer_rate && !update_seller_rate {
        return Ok(());
    };

    // Get rating from message
    let new_rating = msg
        .get_inner_message_kind()
        .get_rating()
        .map_err(MostroInternalErr)?;

    // Get counter to vote from db
    let mut user_to_vote = is_user_present(pool, counterpart.clone())
        .await
        .map_err(|cause| MostroInternalErr(ServiceError::DbAccessError(cause.to_string())))?;

    // Calculate new rating
    user_to_vote.update_rating(new_rating);

    // Create new rating event
    let reputation_event = Rating::new(
        user_to_vote.total_reviews as u64,
        user_to_vote.total_rating as f64,
        user_to_vote.last_rating as u8,
        user_to_vote.min_rating as u8,
        user_to_vote.max_rating as u8,
    )
    .to_tags()
    .map_err(|cause| MostroInternalErr(ServiceError::NostrError(cause.to_string())))?;

    // Save new rating to db
    if let Err(e) = update_user_rating(
        pool,
        user_to_vote.pubkey,
        user_to_vote.last_rating,
        user_to_vote.min_rating,
        user_to_vote.max_rating,
        user_to_vote.total_reviews,
        user_to_vote.total_rating,
    )
    .await
    {
        return Err(MostroInternalErr(ServiceError::DbAccessError(format!(
            "Error updating user rating : {}",
            e
        ))));
    }

    if buyer_rating || seller_rating {
        // Update db with rate flags
        update_user_rating_event(
            &counterpart_trade_pubkey,
            update_buyer_rate,
            update_seller_rate,
            reputation_event,
            &msg,
            my_keys,
            pool,
        )
        .await
        .map_err(|cause| {
            MostroInternalErr(ServiceError::DbAccessError(format!(
                "Error updating user rating event : {}",
                cause
            )))
        })?;

        // Send confirmation message to user that rated
        enqueue_order_msg(
            msg.get_inner_message_kind().request_id,
            Some(order.id),
            Action::RateReceived,
            Some(Payload::RatingUser(new_rating)),
            event.rumor.pubkey,
            None,
        )
        .await;
    }

    Ok(())
}
