use anyhow::Result;
use mostro_core::message::{Action, Message};
use nostr_sdk::prelude::*;
use uuid::Uuid;

use crate::util::send_message_sync;

pub async fn execute_take_dispute(
    dispute_id: &Uuid,
    identity_keys: &Keys,
    trade_keys: &Keys,
    mostro_key: PublicKey,
    client: &Client,
) -> Result<()> {
    println!(
        "Request of take dispute {} from mostro pubId {}",
        dispute_id,
        mostro_key.clone()
    );
    // Create takebuy message
    let take_dispute_message = Message::new_dispute(
        Some(*dispute_id),
        None,
        None,
        Action::AdminTakeDispute,
        None,
    );

    send_message_sync(
        client,
        Some(identity_keys),
        trade_keys,
        mostro_key,
        take_dispute_message,
        true,
        false,
    )
    .await?;

    Ok(())
}
