use crate::app::context::AppContext;
use crate::db::{find_failed_payment_for_master_key, RestoreSessionManager};
use crate::util::{enqueue_order_msg, enqueue_restore_session_msg};
use mostro_core::prelude::*;
use nostr_sdk::prelude::*;

/// Handle restore session action
/// This function starts a background task to process the restore session
/// and immediately returns, avoiding blocking the main application
pub async fn restore_session_action(
    ctx: &AppContext,
    event: &UnwrappedMessage,
) -> Result<(), MostroError> {
    let pool = ctx.pool_arc();
    // Get user master key from the event sender
    let master_key = event.identity.to_string();
    // Get trade key from the event rumor
    let trade_key = event.sender.to_string();

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

    // Start the background processing
    manager
        .start_restore_session(pool.as_ref().clone(), master_key.clone())
        .await?;

    // Start a background task to handle the results
    tokio::spawn(async move {
        handle_restore_session_results(manager, trade_key, master_key, pool).await;
    });

    Ok(())
}

/// Handle restore session results in the background
async fn handle_restore_session_results(
    mut manager: RestoreSessionManager,
    trade_key: String,
    master_key: String,
    pool: std::sync::Arc<sqlx::SqlitePool>,
) {
    // Wait for the result with a timeout
    let timeout = tokio::time::Duration::from_secs(60 * 60); // 1 hour timeout

    match tokio::time::timeout(timeout, manager.wait_for_result()).await {
        Ok(Some(result)) => {
            // Send the restore session response
            if let Err(e) = send_restore_session_response(
                &trade_key,
                &master_key,
                result.restore_orders,
                result.restore_disputes,
                pool.clone(),
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
    master_key: &str,
    orders: Vec<RestoredOrdersInfo>,
    disputes: Vec<RestoredDisputesInfo>,
    pool: std::sync::Arc<sqlx::SqlitePool>,
) -> Result<(), MostroError> {
    // Convert trade_key string to PublicKey
    let trade_pubkey =
        PublicKey::from_hex(trade_key).map_err(|_| MostroCantDo(CantDoReason::InvalidPubkey))?;

    // Collect restored order IDs before moving orders into the payload
    let restored_ids: std::collections::HashSet<uuid::Uuid> =
        orders.iter().map(|o| o.order_id).collect();

    // Send the order data using the flat structure
    enqueue_restore_session_msg(
        Some(Payload::RestoreData(RestoreSessionInfo {
            restore_orders: orders,
            restore_disputes: disputes,
        })),
        trade_pubkey,
    )
    .await;

    tracing::info!("Restore session response sent to user {}", trade_key);

    // Re-send AddInvoice for any orders stuck in settled-hold-invoice with failed payment

    match find_failed_payment_for_master_key(&pool, master_key).await {
        Ok(failed_orders) => {
            for order in failed_orders {
                if restored_ids.contains(&order.id) {
                    enqueue_order_msg(
                        None,
                        Some(order.id),
                        Action::AddInvoice,
                        Some(Payload::Order(SmallOrder::from(order.clone()))),
                        trade_pubkey,
                        None,
                    )
                    .await;
                    tracing::info!(
                        "Re-sent AddInvoice for order {} on restore-session (failed payment)",
                        order.id
                    );
                }
            }
        }
        Err(e) => {
            tracing::error!(
                "Failed to query failed payments during restore-session: {}",
                e
            );
        }
    }

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
