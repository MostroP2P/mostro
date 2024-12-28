use anyhow::Result;
use mostro_core::message::{Action, Message, Payload};
use nostr_sdk::prelude::*;
use uuid::Uuid;

use crate::{
    db::{connect, Order},
    util::send_message_sync,
};

pub async fn execute_rate_user(
    order_id: &Uuid,
    rating: &u8,
    identity_keys: &Keys,
    mostro_key: PublicKey,
    client: &Client,
) -> Result<()> {
    // User rating
    let rating_content;

    // Check boundaries
    if let 1..=5 = *rating {
        rating_content = Payload::RatingUser(*rating);
    } else {
        println!("Rating must be in the range 1 - 5");
        std::process::exit(0);
    }

    let pool = connect().await?;

    let trade_keys = if let Ok(order_to_vote) = Order::get_by_id(&pool, &order_id.to_string()).await
    {
        match order_to_vote.trade_keys.as_ref() {
            Some(trade_keys) => Keys::parse(trade_keys)?,
            None => {
                anyhow::bail!("No trade_keys found for this order");
            }
        }
    } else {
        println!("order {} not found", order_id);
        std::process::exit(0)
    };

    // Create rating message of counterpart
    let rate_message = Message::new_order(
        Some(*order_id),
        None,
        None,
        Action::RateUser,
        Some(rating_content),
    );

    send_message_sync(
        client,
        Some(identity_keys),
        &trade_keys,
        mostro_key,
        rate_message,
        true,
        false,
    )
    .await?;

    std::process::exit(0);
}
