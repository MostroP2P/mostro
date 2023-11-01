use crate::util::{nostr_tags_to_tuple, send_dm, update_user_rating_event};

use anyhow::Result;
use log::error;
use mostro_core::order::Order;
use mostro_core::rating::Rating;
use mostro_core::{Action, Content, Message, NOSTR_REPLACEABLE_EVENT_KIND};
use nostr_sdk::prelude::*;
use sqlx::{Pool, Sqlite};
use sqlx_crud::Crud;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

pub async fn get_counterpart_reputation(
    user: &String,
    my_keys: &Keys,
    client: &Client,
) -> Result<Option<Rating>> {
    // Request NIP33 of the counterparts
    // TODO: filter by data_label=rating generic_tag
    let filters = Filter::new()
        .author(my_keys.public_key().to_string())
        .kind(Kind::Custom(NOSTR_REPLACEABLE_EVENT_KIND))
        .identifier(user.to_string());

    let mut user_reputation_event = client
        .get_events_of(vec![filters], Some(Duration::from_secs(10)))
        .await?;

    // Check if vector of answers is empty
    if user_reputation_event.is_empty() {
        return Ok(None);
    };

    // Sort events by time
    user_reputation_event.sort_by(|a, b| a.created_at.cmp(&b.created_at));
    let tags = nostr_tags_to_tuple(user_reputation_event[0].tags.clone());
    let reputation = Rating::from_tags(tags)?;

    Ok(Some(reputation))
}

pub async fn update_user_reputation_action(
    msg: Message,
    event: &Event,
    my_keys: &Keys,
    client: &Client,
    pool: &Pool<Sqlite>,
    rate_list: Arc<Mutex<Vec<Event>>>,
) -> Result<()> {
    let order_id = msg.order_id.unwrap();
    let order = match Order::by_id(pool, order_id).await? {
        Some(order) => order,
        None => {
            error!("RateUser: Order Id {order_id} not found!");
            return Ok(());
        }
    };

    // Get needed info about users
    let buyer = order.buyer_pubkey.unwrap();
    let seller = order.seller_pubkey.unwrap();
    let message_sender = event.pubkey.to_bech32()?;

    if order.status != "Success" {
        let message = Message::new(0, Some(order.id), None, Action::CantDo, None);
        let message = message.as_json()?;
        send_dm(client, my_keys, &event.pubkey, message).await?;
        error!("RateUser: Order Id {order_id} wrong status");

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
        let message = Message::new(0, Some(order.id), None, Action::CantDo, None);
        let message = message.as_json()?;
        send_dm(client, my_keys, &event.pubkey, message).await?;

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
    let mut rating = 0_u8;

    if let Content::RatingUser(v) = msg.content.unwrap() {
        rating = v;
    }

    // Ask counterpart reputation
    let rep = get_counterpart_reputation(&counterpart, my_keys, client).await?;
    // Here we have to update values of the review of the counterpart
    let mut reputation;
    // min_rate is 1 and max_rate is 5
    let min_rate = 1;
    let max_rate = 5;

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
        let new_rating = format!("{:.2}", new_rating).parse::<f64>().unwrap();

        // Assing new total rating to review
        reputation.total_rating = new_rating;
    } else {
        reputation = Rating::new(1, rating as f64, min_rate, max_rate, rating);
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
        let message = Message::new(0, Some(order.id), None, Action::Received, None);
        let message = message.as_json()?;
        send_dm(client, my_keys, &event.pubkey, message).await?;
    }

    Ok(())
}
