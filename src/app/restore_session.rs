use crate::app::context::AppContext;
use crate::config::settings::get_db_pool;
use crate::db::{find_failed_payment_for_master_key, RestoreSessionManager};
use crate::util::{enqueue_order_msg_on_restore_queue, enqueue_restore_session_msg};
use mostro_core::prelude::*;
use nostr_sdk::prelude::*;

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
        handle_restore_session_results(manager, trade_key, master_key).await;
    });

    Ok(())
}

/// Handle restore session results in the background
async fn handle_restore_session_results(
    mut manager: RestoreSessionManager,
    trade_key: String,
    master_key: String,
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

    tracing::info!("Restore session response sent to user {}", trade_key);

    // Re-send AddInvoice for any orders stuck in settled-hold-invoice with a failed payment.
    // Uses get_db_pool() since pool is not available in this function's scope.
    //
    // `find_failed_payment_for_master_key` filters on `master_buyer_pubkey =
    // master_key`, so every order returned belongs to this restoring user as
    // the BUYER. The correct AddInvoice recipient is therefore the order's
    // own buyer trade key (`order.buyer_pubkey` / get_buyer_pubkey()) — the
    // key the client actually listens on for order DMs. Sending to the
    // master/identity key instead would (a) never reach the client, since
    // order messages are only delivered to trade keys, and (b) publish the
    // identity key as a gift-wrap recipient on Nostr, linking it to this
    // order and breaking trade-key unlinkability.
    let pool = get_db_pool();
    match find_failed_payment_for_master_key(&pool, master_key).await {
        Ok(failed_orders) => {
            for order in failed_orders {
                let buyer_trade_pubkey = match order.get_buyer_pubkey() {
                    Ok(pk) => pk,
                    Err(_) => {
                        tracing::warn!(
                            "Order {} has no valid buyer_pubkey (trade key); skipping AddInvoice re-send",
                            order.id
                        );
                        continue;
                    }
                };
                // Route through the restore-session queue so this AddInvoice
                // is sent AFTER the restore-session response (same queue =
                // FIFO), not before it.
                enqueue_order_msg_on_restore_queue(
                    Some(order.id),
                    Action::AddInvoice,
                    Some(Payload::Order(SmallOrder::from(order.clone()))),
                    buyer_trade_pubkey,
                    order.trade_index_buyer,
                )
                .await;
                tracing::info!(
                    "Re-sent AddInvoice for order {} to buyer trade key {} (failed payment)",
                    order.id,
                    buyer_trade_pubkey
                );
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::context::test_utils::{test_settings, TestContextBuilder};
    use crate::config::MESSAGE_QUEUES;
    use nostr_sdk::Keys;
    use sqlx::SqlitePool;
    use std::sync::Arc;

    async fn create_test_pool() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!().run(&pool).await.unwrap();
        pool
    }

    fn create_event(identity: PublicKey, sender: PublicKey) -> UnwrappedMessage {
        UnwrappedMessage {
            message: Message::new_restore(None),
            signature: None,
            sender,
            identity,
            created_at: Timestamp::now(),
        }
    }

    /// Count queued restore-session messages destined for `dest`. The queue
    /// is a global shared across concurrently running tests, so assertions
    /// always filter by destination key.
    async fn queued_restore_msgs_for(dest: &PublicKey) -> Vec<Message> {
        MESSAGE_QUEUES
            .queue_restore_session_msg
            .read()
            .await
            .iter()
            .filter(|(_, key)| key == dest)
            .map(|(msg, _)| msg.clone())
            .collect()
    }

    /// Happy path: real Nostr keys always stringify as 64-char hex, so both
    /// validation guards pass and the background restore session starts.
    /// (The hex-format guards on `master_key`/`trade_key` are dead code for
    /// real `PublicKey` values — see module report.)
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn restore_session_action_starts_background_restore() {
        let pool = create_test_pool().await;
        let ctx = TestContextBuilder::new()
            .with_pool(Arc::new(pool.clone()))
            .with_settings(test_settings())
            .build();

        let identity = Keys::generate().public_key();
        let trade = Keys::generate().public_key();
        let event = create_event(identity, trade);

        let result = restore_session_action(&ctx, &event).await;

        assert!(result.is_ok());
    }

    /// Drives `handle_restore_session_results` through the `Ok(Some(_))`
    /// branch: the background worker finds no orders/disputes for the master
    /// key and the (empty) restore payload is queued for the trade key.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn handle_restore_session_results_queues_response() {
        let pool = create_test_pool().await;
        let master_key = Keys::generate().public_key().to_string();
        let trade_pubkey = Keys::generate().public_key();
        let trade_key = trade_pubkey.to_string();

        let manager = RestoreSessionManager::new();
        manager
            .start_restore_session(pool.clone(), master_key)
            .await
            .unwrap();

        handle_restore_session_results(manager, trade_key).await;

        let queued = queued_restore_msgs_for(&trade_pubkey).await;
        assert_eq!(queued.len(), 1);
        assert!(matches!(
            queued[0].get_inner_message_kind().payload,
            Some(Payload::RestoreData(_))
        ));
    }

    /// Same flow with an invalid trade key: the result arrives but the
    /// response cannot be built, exercising the logged-error branch.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn handle_restore_session_results_logs_invalid_trade_key() {
        let pool = create_test_pool().await;
        let master_key = Keys::generate().public_key().to_string();

        let manager = RestoreSessionManager::new();
        manager
            .start_restore_session(pool.clone(), master_key)
            .await
            .unwrap();

        // Must not panic; the send failure is logged and swallowed
        handle_restore_session_results(manager, "not-a-hex-key".to_string()).await;
    }

    #[tokio::test]
    async fn send_restore_session_response_queues_message_for_valid_key() {
        let trade_pubkey = Keys::generate().public_key();

        let result =
            send_restore_session_response(&trade_pubkey.to_string(), Vec::new(), Vec::new()).await;

        assert!(result.is_ok());
        let queued = queued_restore_msgs_for(&trade_pubkey).await;
        assert_eq!(queued.len(), 1);
        assert!(matches!(
            queued[0].get_inner_message_kind().payload,
            Some(Payload::RestoreData(_))
        ));
    }

    #[tokio::test]
    async fn send_restore_session_response_rejects_invalid_key() {
        let result = send_restore_session_response("invalid-key", Vec::new(), Vec::new()).await;

        assert!(matches!(
            result,
            Err(MostroCantDo(CantDoReason::InvalidPubkey))
        ));
    }

    #[tokio::test]
    async fn send_restore_session_timeout_queues_message_for_valid_key() {
        let trade_pubkey = Keys::generate().public_key();

        let result = send_restore_session_timeout(&trade_pubkey.to_string()).await;

        assert!(result.is_ok());
        let queued = queued_restore_msgs_for(&trade_pubkey).await;
        assert_eq!(queued.len(), 1);
        assert!(queued[0].get_inner_message_kind().payload.is_none());
    }

    #[tokio::test]
    async fn send_restore_session_timeout_rejects_invalid_key() {
        let result = send_restore_session_timeout("invalid-key").await;

        assert!(matches!(
            result,
            Err(MostroCantDo(CantDoReason::InvalidPubkey))
        ));
    }
}
