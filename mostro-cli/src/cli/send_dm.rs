use crate::{db::Order, util::send_message_sync};
use anyhow::Result;
use mostro_core::message::{Action, Message, Payload};
use nostr_sdk::prelude::*;
use uuid::Uuid;

pub async fn execute_send_dm(
    receiver: PublicKey,
    client: &Client,
    order_id: &Uuid,
    message: &str,
) -> Result<()> {
    let message = Message::new_dm(
        None,
        None,
        Action::SendDm,
        Some(Payload::TextMessage(message.to_string())),
    );

    let pool = crate::db::connect().await?;

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

    send_message_sync(client, None, &trade_keys, receiver, message, true, true).await?;

    Ok(())
}
