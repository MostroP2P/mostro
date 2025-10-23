use crate::{db::RestoreSessionManager, util::enqueue_restore_session_msg};
use mostro_core::prelude::*;
use nostr::nips::nip59::UnwrappedGift;
use nostr_sdk::prelude::*;
use sqlx::{Pool, Sqlite};

/// Handle restore session action
/// This function starts a background task to process the restore session
/// and immediately returns, avoiding blocking the main application
pub async fn restore_session_action(
    event: &UnwrappedGift,
    pool: &Pool<Sqlite>,
) -> Result<(), MostroError> {
    // Get user master key from the event sender
    let master_key = event.sender.to_string();
    // Get trade key from the event rumor
    let trade_key = event.rumor.pubkey.to_string();

    // Validate the master key format
    if !master_key.chars().all(|c| c.is_ascii_hexdigit()) || master_key.len() != 64 {
        return Err(MostroCantDo(CantDoReason::InvalidPubkey));
    }

    // Validate the trade key format
    if !trade_key.chars().all(|c| c.is_ascii_hexdigit()) || trade_key.len() != 64 {
        return Err(MostroCantDo(CantDoReason::InvalidPubkey));
    }

    tracing::info!(
        "Starting background restore session for master key: {}",
        master_key
    );

    // Create a new manager for this specific restore session
    let manager = RestoreSessionManager::new();
    let pool_clone = pool.clone();

    // Start the background processing
    manager
        .start_restore_session(pool_clone, master_key.clone())
        .await?;

    // Start a background task to handle the results
    tokio::spawn(async move {
        handle_restore_session_results(manager, trade_key).await;
    });

    Ok(())
}

/// Handle restore session results in the background
async fn handle_restore_session_results(mut manager: RestoreSessionManager, trade_key: String) {
    // Wait for the result with a timeout
    let timeout = tokio::time::Duration::from_secs(60 * 60); // 1 hour timeout

    match tokio::time::timeout(timeout, manager.wait_for_result()).await {
        Ok(Some(result)) => {
            // Send the restore session response
            if let Err(e) = send_restore_session_response(
                &trade_key,
                result.restore_orders,
                result.restore_disputes,
            )
            .await
            {
                tracing::error!("Failed to send restore session response: {}", e);
            }
        }
        Ok(None) => {
            tracing::error!("Restore session result channel closed unexpectedly");
        }
        Err(_) => {
            tracing::error!("Restore session timed out after 1 hour");
            // Send timeout message to user
            if let Err(e) = send_restore_session_timeout(&trade_key).await {
                tracing::error!("Failed to send timeout message: {}", e);
            }
        }
    }
}

/// Send restore session response to the user
async fn send_restore_session_response(
    trade_key: &str,
    orders: Vec<RestoredOrdersInfo>,
    disputes: Vec<RestoredDisputesInfo>,
) -> Result<(), MostroError> {
    // Convert trade_key string to PublicKey
    let trade_pubkey =
        PublicKey::from_hex(trade_key).map_err(|_| MostroCantDo(CantDoReason::InvalidPubkey))?;

    // Send the order data using the flat structure
    enqueue_restore_session_msg(
        Some(Payload::RestoreData(RestoreSessionInfo {
            restore_orders: orders,
            restore_disputes: disputes,
        })),
        trade_pubkey,
    )
    .await;

    tracing::info!("Restore session response sent to user {}", trade_key,);

    Ok(())
}

/// Send timeout message to user
async fn send_restore_session_timeout(trade_key: &str) -> Result<(), MostroError> {
    let trade_pubkey =
        PublicKey::from_hex(trade_key).map_err(|_| MostroCantDo(CantDoReason::InvalidPubkey))?;

    // Send timeout message without payload since Text doesn't exist
    enqueue_restore_session_msg(None, trade_pubkey).await;

    tracing::warn!("Restore session timed out for user: {}", trade_key);

    Ok(())
}
