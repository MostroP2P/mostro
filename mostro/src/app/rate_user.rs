use crate::util::{send_cant_do_msg, send_new_order_msg, update_user_rating_event};
use crate::NOSTR_CLIENT;

use crate::db::{is_user_present, update_user_rating};
use anyhow::{Error, Result};
use mostro_core::message::{Action, CantDoReason, Message, Payload};
use mostro_core::order::{Order, Status};
use mostro_core::rating::Rating;
use mostro_core::NOSTR_REPLACEABLE_EVENT_KIND;
use nostr::nips::nip59::UnwrappedGift;
use nostr_sdk::prelude::*;
use sqlx::{Pool, Sqlite};
use sqlx_crud::Crud;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tracing::error;

pub const MAX_RATING: u8 = 5;
pub const MIN_RATING: u8 = 1;

pub async fn get_user_reputation(user: &str, my_keys: &Keys) -> Result<Option<Rating>> {
    // Request NIP33 of the counterparts
    let filters = Filter::new()
        .author(my_keys.public_key())
        .kind(Kind::ParameterizedReplaceable(NOSTR_REPLACEABLE_EVENT_KIND))
        .custom_tag(SingleLetterTag::lowercase(Alphabet::Z), vec!["rating"])
        .identifier(user.to_string());

    let mut user_reputation_event = NOSTR_CLIENT
        .get()
        .unwrap()
        .fetch_events(vec![filters], Some(Duration::from_secs(10)))
        .await?
        .to_vec();

    // Check if vector of answers is empty
    if user_reputation_event.is_empty() {
        return Ok(None);
    };

    // Sort events by time
    user_reputation_event.sort_by(|a, b| a.created_at.cmp(&b.created_at));
    let tags = user_reputation_event[0].tags.clone();
    let reputation = Rating::from_tags(tags)?;

    Ok(Some(reputation))
}

pub async fn update_user_reputation_action(
    msg: Message,
    event: &UnwrappedGift,
    my_keys: &Keys,
    pool: &Pool<Sqlite>,
    rate_list: Arc<Mutex<Vec<Event>>>,
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

    // Get needed info about users
    let (seller, buyer) = match (&order.seller_pubkey, &order.buyer_pubkey) {
        (Some(seller), Some(buyer)) => (seller.to_owned(), buyer.to_owned()),
        (None, _) => return Err(Error::msg("Missing seller pubkey")),
        (_, None) => return Err(Error::msg("Missing buyer pubkey")),
    };

    let message_sender = event.rumor.pubkey.to_string();

    if order.status != Status::Success.to_string() {
        send_cant_do_msg(
            request_id,
            Some(order.id),
            Some(CantDoReason::InvalidOrderStatus),
            &event.rumor.pubkey,
        )
        .await;
        error!("Order Id {order_id} wrong status");
        return Ok(());
    }
    // Get counterpart pubkey
    let mut counterpart: String = String::new();
    let mut counterpart_trade_pubkey: String = String::new();
    let mut buyer_rating: bool = false;
    let mut seller_rating: bool = false;

    // Find the counterpart public key
    if message_sender == buyer {
        counterpart = order
            .master_seller_pubkey
            .ok_or_else(|| Error::msg("Missing seller identity pubkey"))?;
        buyer_rating = true;
        counterpart_trade_pubkey = order
            .buyer_pubkey
            .ok_or_else(|| Error::msg("Missing buyer pubkey"))?;
    } else if message_sender == seller {
        counterpart = order
            .master_buyer_pubkey
            .ok_or_else(|| Error::msg("Missing buyer identity pubkey"))?;
        seller_rating = true;
        counterpart_trade_pubkey = order
            .seller_pubkey
            .ok_or_else(|| Error::msg("Missing seller pubkey"))?;
    };

    // Add a check in case of no counterpart found
    if counterpart.is_empty() {
        // We create a Message
        send_cant_do_msg(
            request_id,
            Some(order.id),
            Some(CantDoReason::InvalidPeer),
            &event.rumor.pubkey,
        )
        .await;
        return Ok(());
    };

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

    // Check if content of Peer is the same of counterpart
    let rating =
        if let Some(Payload::RatingUser(v)) = msg.get_inner_message_kind().payload.to_owned() {
            if !(MIN_RATING..=MAX_RATING).contains(&v) {
                return Err(Error::msg(format!(
                    "Rating must be between {} and {}",
                    MIN_RATING, MAX_RATING
                )));
            }
            v
        } else {
            return Err(Error::msg("No rating present"));
        };

    // Get counter to vote from db
    let mut user_to_vote = is_user_present(pool, counterpart.clone()).await?;

    // Update user reputation
    // Going on with calculation
    // increment first
    user_to_vote.total_reviews += 1;
    let old_rating = user_to_vote.total_rating as f64;
    // recompute new rating
    if user_to_vote.total_reviews <= 1 {
        user_to_vote.total_rating = rating.into();
        user_to_vote.max_rating = rating.into();
        user_to_vote.min_rating = rating.into();
    } else {
        user_to_vote.total_rating = old_rating
            + ((user_to_vote.last_rating as f64) - old_rating)
                / (user_to_vote.total_reviews as f64);
        if user_to_vote.max_rating < rating.into() {
            user_to_vote.max_rating = rating.into();
        }
        if user_to_vote.min_rating > rating.into() {
            user_to_vote.min_rating = rating.into();
        }
    }
    // Store last rating
    user_to_vote.last_rating = rating.into();
    // Create new rating event
    let reputation_event = Rating::new(
        user_to_vote.total_reviews as u64,
        user_to_vote.total_rating as f64,
        user_to_vote.last_rating as u8,
        user_to_vote.min_rating as u8,
        user_to_vote.max_rating as u8,
    )
    .to_tags()?;

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
        return Err(Error::msg(format!("Error updating user rating : {}", e)));
    }

    if buyer_rating || seller_rating {
        // Update db with rate flags
        update_user_rating_event(
            &counterpart_trade_pubkey,
            update_buyer_rate,
            update_seller_rate,
            reputation_event,
            order.id,
            my_keys,
            pool,
            rate_list,
        )
        .await?;

        // Send confirmation message to user that rated
        send_new_order_msg(
            msg.get_inner_message_kind().request_id,
            Some(order.id),
            Action::RateReceived,
            Some(Payload::RatingUser(rating)),
            &event.rumor.pubkey,
            None,
        )
        .await;
    }

    Ok(())
}
