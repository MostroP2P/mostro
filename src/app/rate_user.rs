use crate::util::{send_cant_do_msg, send_new_order_msg, update_user_rating_event};
use crate::NOSTR_CLIENT;

use anyhow::{Error, Result};
use mostro_core::message::{Action, Message, Payload};
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

const MAX_RATING: u8 = 5;
const MIN_RATING: u8 = 1;

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
        send_cant_do_msg(request_id, Some(order.id), None, &event.rumor.pubkey).await;
        error!("Order Id {order_id} wrong status");
        return Ok(());
    }
    // Get counterpart pubkey
    let mut counterpart: String = String::new();
    let mut buyer_rating: bool = false;
    let mut seller_rating: bool = false;

    // Find the counterpart public key
    if message_sender == buyer {
        counterpart = seller;
        buyer_rating = true;
    } else if message_sender == seller {
        counterpart = buyer;
        seller_rating = true;
    };

    // Add a check in case of no counterpart found
    if counterpart.is_empty() {
        // We create a Message
        send_cant_do_msg(request_id, Some(order.id), None, &event.rumor.pubkey).await;
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
    let rating;

    if let Some(Payload::RatingUser(v)) = msg.get_inner_message_kind().payload.to_owned() {
        if !(MIN_RATING..=MAX_RATING).contains(&v) {
            return Err(Error::msg(format!(
                "Rating must be between {} and {}",
                MIN_RATING, MAX_RATING
            )));
        }
        rating = v;
    } else {
        return Err(Error::msg("No rating present"));
    }

    // Ask counterpart reputation
    let rep = get_user_reputation(&counterpart, my_keys).await?;
    // Here we have to update values of the review of the counterpart
    let mut reputation;

    if let Some(r) = rep {
        // Update user reputation
        // Going on with calculation
        reputation = r;
        let old_rating = reputation.total_rating;
        let last_rating = reputation.last_rating;
        let new_rating =
            old_rating + (last_rating as f64 - old_rating) / (reputation.total_reviews as f64);

        reputation.last_rating = rating;
        reputation.total_reviews += 1;
        // Format with two decimals
        let new_rating = format!("{:.2}", new_rating).parse::<f64>()?;

        // Assing new total rating to review
        reputation.total_rating = new_rating;
    } else {
        reputation = Rating::new(1, rating as f64, rating, MIN_RATING, MAX_RATING);
    }
    let reputation = reputation.to_tags()?;

    if buyer_rating || seller_rating {
        // Update db with rate flags
        update_user_rating_event(
            &counterpart,
            update_buyer_rate,
            update_seller_rate,
            reputation,
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
