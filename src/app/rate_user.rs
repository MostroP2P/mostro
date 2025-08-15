use crate::config::MOSTRO_DB_PASSWORD;
use crate::db::{is_user_present, update_user_rating};
use crate::util::{enqueue_order_msg, get_order, update_user_rating_event};
use mostro_core::prelude::*;
use nostr::nips::nip59::UnwrappedGift;
use nostr_sdk::prelude::*;
use sqlx::{Pool, Sqlite};

pub fn prepare_variables_for_vote(
    message_sender: &str,
    order: &Order,
) -> Result<(String, bool, bool), MostroError> {
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
        buyer_rating = true;
        counterpart_trade_pubkey = order
            .get_buyer_pubkey()
            .map_err(MostroInternalErr)?
            .to_string();
    } else if message_sender == seller {
        seller_rating = true;
        counterpart_trade_pubkey = order
            .get_seller_pubkey()
            .map_err(MostroInternalErr)?
            .to_string();
    };

    Ok((counterpart_trade_pubkey, buyer_rating, seller_rating))
}

/// Updates a user's reputation based on a rating received from a trade counterpart.
///
/// This function handles the reputation update process for users after a successful trade.
/// It processes ratings from either the buyer or seller of a completed order and updates
/// the recipient's reputation metrics accordingly. The function also handles privacy mode
/// checks and ensures users can only rate their trade counterpart once.
///
/// # Arguments
///
/// * `msg` - The message containing the rating information
/// * `event` - The unwrapped gift event containing the sender's information
/// * `my_keys` - The keys used for signing events
/// * `pool` - The database connection pool
///
/// # Returns
///
/// * `Result<(), MostroError>` - Returns `Ok(())` if the reputation update was successful,
///   or an appropriate error if something went wrong during the process.
///
/// # Process Flow
///
/// 1. Retrieves the order information from the database
/// 2. Verifies the order status is "Success"
/// 3. Determines if the rating is from buyer or seller
/// 4. Checks if the user has already rated their counterpart
/// 5. Validates privacy mode settings
/// 6. Updates the recipient's rating metrics
/// 7. Creates and saves a new rating event
/// 8. Updates the database with the new rating information
/// 9. Sends a confirmation message to the rating user
pub async fn update_user_reputation_action(
    msg: Message,
    event: &UnwrappedGift,
    my_keys: &Keys,
    pool: &Pool<Sqlite>,
) -> Result<(), MostroError> {
    // Get order
    let order = get_order(&msg, pool).await?;

    // Prepare variables for vote
    let (counterpart_trade_pubkey, buyer_rating, seller_rating) =
        prepare_variables_for_vote(&event.rumor.pubkey.to_string(), &order)?;

    // Check if order is success, but sellers can rate in status settled-hold-invoice
    if !(order.check_status(Status::Success).is_ok()
        || (order.check_status(Status::SettledHoldInvoice).is_ok() && seller_rating))
    {
        return Err(MostroCantDo(CantDoReason::InvalidOrderStatus));
    }

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

    // Check if users are in full privacy mode
    let (normal_buyer_idkey, normal_seller_idkey) = order
        .is_full_privacy_order(MOSTRO_DB_PASSWORD.get())
        .map_err(|_| MostroInternalErr(ServiceError::InvalidPubkey))?;

    // Get counter to vote from db, but only if they're not in privacy mode
    let mut user_to_vote = if buyer_rating {
        // If buyer is rating seller, check if seller is in privacy mode
        if let Some(seller_key) = normal_seller_idkey {
            is_user_present(pool, seller_key).await.map_err(|cause| {
                MostroInternalErr(ServiceError::DbAccessError(cause.to_string()))
            })?
        } else {
            return Ok(());
        }
    } else {
        // If seller is rating buyer, check if buyer is in privacy mode
        if let Some(buyer_key) = normal_buyer_idkey {
            is_user_present(pool, buyer_key).await.map_err(|cause| {
                MostroInternalErr(ServiceError::DbAccessError(cause.to_string()))
            })?
        } else {
            return Ok(());
        }
    };

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
