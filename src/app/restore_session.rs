use crate::app::context::AppContext;
use crate::{
    db::RestoreSessionManager,
    util::{enqueue_restore_session_msg, is_valid_hex_pubkey},
};
use mostro_core::prelude::*;
use nostr_sdk::prelude::*;

/// Restore session results wait for this long before the requester is told
/// to retry instead of hanging forever.
const RESTORE_SESSION_TIMEOUT_SECS: u64 = 60 * 60;

/// Handle restore session action
/// This function starts a background task to process the restore session
/// and immediately returns, avoiding blocking the main application
pub async fn restore_session_action(
    ctx: &AppContext,
    event: &UnwrappedMessage,
) -> Result<(), MostroError> {
    let pool = ctx.pool();
    // Get user master key from the event sender
    let master_key = event.identity.to_string();
    // Get trade key from the event rumor
    let trade_key = event.sender.to_string();

    // Validate the master key format
    if !is_valid_hex_pubkey(&master_key) {
        return Err(MostroCantDo(CantDoReason::InvalidPubkey));
    }

    // Validate the trade key format
    if !is_valid_hex_pubkey(&trade_key) {
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
    let timeout = tokio::time::Duration::from_secs(RESTORE_SESSION_TIMEOUT_SECS);

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restore_session_timeout_is_one_hour() {
        assert_eq!(RESTORE_SESSION_TIMEOUT_SECS, 3600);
    }

    #[tokio::test]
    async fn send_restore_session_response_rejects_invalid_trade_key() {
        let err = send_restore_session_response("not-a-pubkey", vec![], vec![])
            .await
            .unwrap_err();
        assert_eq!(err, MostroError::MostroCantDo(CantDoReason::InvalidPubkey));
    }

    #[tokio::test]
    async fn send_restore_session_timeout_rejects_invalid_trade_key() {
        let err = send_restore_session_timeout("not-a-pubkey")
            .await
            .unwrap_err();
        assert_eq!(err, MostroError::MostroCantDo(CantDoReason::InvalidPubkey));
    }
}
