use crate::db::is_user_present;
use crate::util::send_dm;
use mostro_core::prelude::*;
use nostr::nips::nip59::UnwrappedGift;
use nostr_sdk::prelude::*;
use sqlx::{Pool, Sqlite};

// Handle synch_user_trade_index action
pub async fn synch_last_trade_index(
    event: &UnwrappedGift,
    my_keys: &Keys,
    pool: &Pool<Sqlite>,
) -> Result<(), MostroError> {
    // Get requester pubkey (sender of the message)
    let requester_pubkey = event.sender.to_string();

    // Fetch user to read last_trade_index
    let user = is_user_present(pool, requester_pubkey).await?;

    // Build response message embedding the last_trade_index in the trade_index field
    let last_trade_idx_message = MessageKind::new(
        None,
        None,
        Some(user.last_trade_index),
        Action::LastTradeIndex,
        None,
    )
    .as_json()
    .map_err(|_| MostroError::MostroInternalErr(ServiceError::MessageSerializationError))?;

    // Send DM back to the requester
    send_dm(event.sender, my_keys, &last_trade_idx_message, None).await?;

    Ok(())
}
