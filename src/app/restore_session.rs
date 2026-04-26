use crate::app::context::AppContext;
use crate::{db::RestoreSessionManager, util::enqueue_restore_session_msg};
use mostro_core::prelude::*;
use nostr_sdk::prelude::*;
use sqlx::{Pool, Sqlite};

// SQL queries should be fast so minute duration is enough
const RESTORE_SESSION_TIMEOUT_SECS: u64 = 60; // 1 minute

fn redact_pubkey(key: &str) -> String {
    if key.len() <= 16 {
        return "<redacted>".to_string();
    }

    format!("{}...{}", &key[..8], &key[key.len() - 8..])
}

/// Handle restore session action
/// This function starts a background task to process the restore session
/// and immediately returns, avoiding blocking the main application
pub async fn restore_session_action(
    ctx: &AppContext,
    event: &UnwrappedMessage,
) -> Result<(), MostroError> {
    restore_session_core(event, ctx.pool()).await
}

/// Validates the sender and rumor public keys from `event`, then starts a
/// [`RestoreSessionManager`] to query the database and spawns a background
/// task that waits for the result and delivers it back to the user's trade key.
pub async fn restore_session_core(
    event: &UnwrappedGift,
    pool: &Pool<Sqlite>,
) -> Result<(), MostroError> {
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
        redact_pubkey(&master_key)
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
        Ok(Some(Ok(result))) => {
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
        Ok(Some(Err(e))) => {
            tracing::error!("Restore session processing failed: {}", e);
            if let Err(timeout_err) = send_restore_session_timeout(&trade_key).await {
                tracing::error!(
                    "Failed to send restore session failure message: {}",
                    timeout_err
                );
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

    tracing::info!(
        "Restore session response sent to user {}",
        redact_pubkey(trade_key),
    );

    Ok(())
}

/// Send timeout message to user
async fn send_restore_session_timeout(trade_key: &str) -> Result<(), MostroError> {
    let trade_pubkey =
        PublicKey::from_hex(trade_key).map_err(|_| MostroCantDo(CantDoReason::InvalidPubkey))?;

    // Send timeout message without payload since Text doesn't exist
    enqueue_restore_session_msg(None, trade_pubkey).await;

    tracing::warn!(
        "Restore session timed out for user: {}",
        redact_pubkey(trade_key)
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::context::test_utils::{test_settings, TestContextBuilder};
    use crate::config::MESSAGE_QUEUES;
    use mostro_core::prelude::CantDoReason;
    use nostr::nips::nip59::UnwrappedGift;
    use nostr_sdk::prelude::{Kind as NostrKind, PublicKey, Timestamp, UnsignedEvent};
    use sqlx::sqlite::SqlitePoolOptions;
    use std::sync::Arc;
    use tokio::time::{sleep, Duration};

    const VALID_MASTER_KEY: &str =
        "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2";
    const VALID_TRADE_KEY_CORE: &str =
        "b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3";
    const VALID_TRADE_KEY_ACTION: &str =
        "d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5";
    /// A valid-format key that deliberately does not match any order's
    /// `master_buyer_pubkey` or `master_seller_pubkey`, used to verify that
    /// unrelated senders are denied access to another user's data.
    const STRANGER_KEY: &str = "c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4";

    /// Constructs an [`UnwrappedGift`] from raw hex public keys for use in tests.
    fn make_unwrapped_gift(sender_hex: &str, rumor_pubkey_hex: &str) -> UnwrappedGift {
        let sender = PublicKey::from_hex(sender_hex).unwrap();
        let rumor_pubkey = PublicKey::from_hex(rumor_pubkey_hex).unwrap();
        let rumor =
            UnsignedEvent::new(rumor_pubkey, Timestamp::now(), NostrKind::Custom(4), [], "");
        UnwrappedGift { sender, rumor }
    }

    /// Creates an in-memory SQLite pool and runs production migrations.
    async fn setup_restore_db() -> sqlx::SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();

        sqlx::migrate!().run(&pool).await.unwrap();

        pool
    }

    /// Inserts a buy-side order row whose `master_buyer_pubkey` and
    /// `trade_index_buyer` are set to the supplied values.
    async fn insert_order_with_master_buyer(
        pool: &sqlx::SqlitePool,
        id: uuid::Uuid,
        status: &str,
        master_buyer_pubkey: &str,
        trade_index_buyer: i64,
    ) {
        sqlx::query(
            r#"INSERT INTO orders (id, kind, event_id, status, premium, payment_method,
                    amount, fiat_code, fiat_amount, created_at, expires_at,
                    failed_payment, payment_attempts, dev_fee, dev_fee_paid,
                    master_buyer_pubkey, trade_index_buyer)
            VALUES (?1, 'buy', 'ev1', ?2, 0, 'lightning',
                    100000, 'USD', 100, 1700000000, 1700086400,
                    0, 0, 0, 0, ?3, ?4)"#,
        )
        .bind(id)
        .bind(status)
        .bind(master_buyer_pubkey)
        .bind(trade_index_buyer)
        .execute(pool)
        .await
        .unwrap();
    }

    /// Inserts a sell-side order row whose `master_seller_pubkey` and
    /// `trade_index_seller` are set to the supplied values.
    async fn insert_order_with_master_seller(
        pool: &sqlx::SqlitePool,
        id: uuid::Uuid,
        status: &str,
        master_seller_pubkey: &str,
        trade_index_seller: i64,
    ) {
        sqlx::query(
            r#"INSERT INTO orders (id, kind, event_id, status, premium, payment_method,
                    amount, fiat_code, fiat_amount, created_at, expires_at,
                    failed_payment, payment_attempts, dev_fee, dev_fee_paid,
                    master_seller_pubkey, trade_index_seller)
            VALUES (?1, 'sell', 'ev1', ?2, 0, 'lightning',
                    100000, 'USD', 100, 1700000000, 1700086400,
                    0, 0, 0, 0, ?3, ?4)"#,
        )
        .bind(id)
        .bind(status)
        .bind(master_seller_pubkey)
        .bind(trade_index_seller)
        .execute(pool)
        .await
        .unwrap();
    }

    /// Inserts a dispute row linked to `order_id` with the given `status`.
    async fn insert_dispute(
        pool: &sqlx::SqlitePool,
        dispute_id: uuid::Uuid,
        order_id: uuid::Uuid,
        status: &str,
    ) {
        sqlx::query(
            r#"INSERT INTO disputes (id, order_id, status, order_previous_status, created_at)
            VALUES (?1, ?2, ?3, 'active', 1700000000)"#,
        )
        .bind(dispute_id)
        .bind(order_id)
        .bind(status)
        .execute(pool)
        .await
        .unwrap();
    }

    // --- restore_session_action ---

    async fn wait_for_restore_session_msg_for_key(trade_key: &str) {
        let trade_pubkey = PublicKey::from_hex(trade_key).expect("trade key should be valid hex");
        let timeout = Duration::from_secs(2);
        let start = tokio::time::Instant::now();

        while start.elapsed() < timeout {
            let queued_for_key = MESSAGE_QUEUES
                .queue_restore_session_msg
                .read()
                .await
                .iter()
                .any(|(_, destination)| *destination == trade_pubkey);
            if queued_for_key {
                return;
            }
            sleep(Duration::from_millis(10)).await;
        }

        panic!("Timed out waiting for restore-session message for target key");
    }

    #[tokio::test]
    async fn restore_session_core_enqueues_message_for_valid_keys() {
        let pool = setup_restore_db().await;
        let gift = make_unwrapped_gift(VALID_MASTER_KEY, VALID_TRADE_KEY_CORE);

        let result = restore_session_core(&gift, &pool).await;
        assert!(
            result.is_ok(),
            "Should accept well-formed keys: {:?}",
            result
        );
        wait_for_restore_session_msg_for_key(VALID_TRADE_KEY_CORE).await;
    }

    #[tokio::test]
    async fn restore_session_action_enqueues_message_for_valid_keys() {
        let pool = Arc::new(setup_restore_db().await);
        let ctx = TestContextBuilder::new()
            .with_pool(pool)
            .with_settings(test_settings())
            .build();
        let gift = make_unwrapped_gift(VALID_MASTER_KEY, VALID_TRADE_KEY_ACTION);

        let result = restore_session_action(&ctx, &gift).await;
        assert!(
            result.is_ok(),
            "Expected restore_session_action to succeed with valid keys: {:?}",
            result
        );
        wait_for_restore_session_msg_for_key(VALID_TRADE_KEY_ACTION).await;
    }

    // --- find_user_orders_by_master_key validation ---

    #[tokio::test]
    async fn find_user_orders_rejects_short_master_key() {
        let pool = setup_restore_db().await;
        let result = crate::db::find_user_orders_by_master_key(&pool, "tooshort").await;
        assert!(
            matches!(result, Err(MostroCantDo(CantDoReason::InvalidPubkey))),
            "Should reject short master key"
        );
    }

    #[tokio::test]
    async fn find_user_orders_rejects_non_hex_master_key() {
        let pool = setup_restore_db().await;
        let mut bad_key = VALID_MASTER_KEY.to_string();
        bad_key.replace_range(63..64, "g"); // keep length 64, make last char non-hex
        let result = crate::db::find_user_orders_by_master_key(&pool, &bad_key).await;
        assert!(
            matches!(result, Err(MostroCantDo(CantDoReason::InvalidPubkey))),
            "Should reject non-hex master key"
        );
    }

    #[tokio::test]
    async fn find_user_orders_rejects_too_long_master_key() {
        let pool = setup_restore_db().await;
        let long_key = "a".repeat(65);
        let result = crate::db::find_user_orders_by_master_key(&pool, &long_key).await;
        assert!(
            matches!(result, Err(MostroCantDo(CantDoReason::InvalidPubkey))),
            "Should reject 65-char key"
        );
    }

    #[tokio::test]
    async fn finds_active_buyer_order_by_master_key() {
        let pool = setup_restore_db().await;
        let order_id = uuid::Uuid::new_v4();
        insert_order_with_master_buyer(&pool, order_id, "active", VALID_MASTER_KEY, 3).await;

        let result = crate::db::find_user_orders_by_master_key(&pool, VALID_MASTER_KEY)
            .await
            .unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].order_id, order_id);
        assert_eq!(result[0].trade_index, 3);
        assert_eq!(result[0].status, "active");
    }

    #[tokio::test]
    async fn finds_active_seller_order_by_master_key() {
        let pool = setup_restore_db().await;
        let order_id = uuid::Uuid::new_v4();
        insert_order_with_master_seller(&pool, order_id, "waiting-payment", VALID_MASTER_KEY, 7)
            .await;

        let result = crate::db::find_user_orders_by_master_key(&pool, VALID_MASTER_KEY)
            .await
            .unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].order_id, order_id);
        assert_eq!(result[0].trade_index, 7);
    }

    #[tokio::test]
    async fn excludes_terminal_statuses_from_order_restore() {
        let pool = setup_restore_db().await;
        let terminal_statuses = [
            "expired",
            "success",
            "canceled",
            "dispute",
            "canceledbyadmin",
            "completedbyadmin",
            "settledbyadmin",
            "cooperativelycanceled",
        ];
        for status in &terminal_statuses {
            insert_order_with_master_buyer(
                &pool,
                uuid::Uuid::new_v4(),
                status,
                VALID_MASTER_KEY,
                1,
            )
            .await;
        }

        let result = crate::db::find_user_orders_by_master_key(&pool, VALID_MASTER_KEY)
            .await
            .unwrap();
        assert!(
            result.is_empty(),
            "All terminal-status orders should be excluded"
        );
    }

    #[tokio::test]
    async fn returns_only_orders_for_queried_master_key() {
        let pool = setup_restore_db().await;
        let other_key = VALID_TRADE_KEY_CORE;
        insert_order_with_master_buyer(&pool, uuid::Uuid::new_v4(), "active", VALID_MASTER_KEY, 1)
            .await;
        insert_order_with_master_buyer(&pool, uuid::Uuid::new_v4(), "active", other_key, 2).await;

        let result = crate::db::find_user_orders_by_master_key(&pool, VALID_MASTER_KEY)
            .await
            .unwrap();
        assert_eq!(result.len(), 1, "Should only return orders for queried key");
    }

    #[tokio::test]
    async fn returns_empty_when_no_orders_for_master_key() {
        let pool = setup_restore_db().await;

        let result = crate::db::find_user_orders_by_master_key(&pool, VALID_MASTER_KEY)
            .await
            .unwrap();
        assert!(result.is_empty());
    }

    // --- permission checks (who can restore what) ---

    /// A sender whose key does not appear in any order's `master_buyer_pubkey`
    /// or `master_seller_pubkey` must receive zero results for orders.
    /// This test directly kills mutants that remove or weaken the ownership
    /// `WHERE` clause inside `find_user_orders_by_master_key`.
    #[tokio::test]
    async fn stranger_sender_cannot_see_another_users_orders() {
        let pool = setup_restore_db().await;

        // Seed one buy-side and one sell-side order, both owned by VALID_MASTER_KEY.
        insert_order_with_master_buyer(&pool, uuid::Uuid::new_v4(), "active", VALID_MASTER_KEY, 1)
            .await;
        insert_order_with_master_seller(
            &pool,
            uuid::Uuid::new_v4(),
            "waiting-payment",
            VALID_MASTER_KEY,
            2,
        )
        .await;

        // Query as a completely unrelated key – must return nothing.
        let result = crate::db::find_user_orders_by_master_key(&pool, STRANGER_KEY)
            .await
            .unwrap();
        assert!(
            result.is_empty(),
            "A stranger key must not return orders owned by a different master key; \
             got {} record(s)",
            result.len()
        );
    }

    /// A sender whose key does not appear in any order's `master_buyer_pubkey`
    /// or `master_seller_pubkey` must receive zero results for disputes.
    /// This test directly kills mutants that remove or weaken the ownership
    /// `WHERE` clause inside `find_user_disputes_by_master_key`.
    #[tokio::test]
    async fn stranger_sender_cannot_see_another_users_disputes() {
        let pool = setup_restore_db().await;
        let order_id = uuid::Uuid::new_v4();

        // Seed a dispute linked to VALID_MASTER_KEY as the buyer.
        sqlx::query(
            r#"INSERT INTO orders (id, kind, event_id, status, premium, payment_method,
                    amount, fiat_code, fiat_amount, created_at, expires_at,
                    failed_payment, payment_attempts, dev_fee, dev_fee_paid,
                    master_buyer_pubkey, trade_index_buyer, buyer_dispute, seller_dispute)
            VALUES (?1, 'buy', 'ev1', 'dispute', 0, 'lightning',
                    100000, 'USD', 100, 1700000000, 1700086400,
                    0, 0, 0, 0, ?2, 3, 1, 0)"#,
        )
        .bind(order_id)
        .bind(VALID_MASTER_KEY)
        .execute(&pool)
        .await
        .unwrap();
        insert_dispute(&pool, uuid::Uuid::new_v4(), order_id, "initiated").await;

        // Query as a completely unrelated key – must return nothing.
        let result = crate::db::find_user_disputes_by_master_key(&pool, STRANGER_KEY)
            .await
            .unwrap();
        assert!(
            result.is_empty(),
            "A stranger key must not return disputes owned by a different master key; \
             got {} record(s)",
            result.len()
        );
    }

    // --- find_user_disputes_by_master_key ---

    #[tokio::test]
    async fn rejects_invalid_key_for_disputes_query() {
        let pool = setup_restore_db().await;
        let result = crate::db::find_user_disputes_by_master_key(&pool, "bad").await;
        assert!(
            matches!(result, Err(MostroCantDo(CantDoReason::InvalidPubkey))),
            "Should reject invalid key for disputes"
        );
    }

    #[tokio::test]
    async fn finds_active_dispute_for_buyer() {
        let pool = setup_restore_db().await;
        let order_id = uuid::Uuid::new_v4();
        let dispute_id = uuid::Uuid::new_v4();

        sqlx::query(
            r#"INSERT INTO orders (id, kind, event_id, status, premium, payment_method,
                    amount, fiat_code, fiat_amount, created_at, expires_at,
                    failed_payment, payment_attempts, dev_fee, dev_fee_paid,
                    master_buyer_pubkey, trade_index_buyer, buyer_dispute, seller_dispute)
            VALUES (?1, 'buy', 'ev1', 'dispute', 0, 'lightning',
                    100000, 'USD', 100, 1700000000, 1700086400,
                    0, 0, 0, 0, ?2, 5, 1, 0)"#,
        )
        .bind(order_id)
        .bind(VALID_MASTER_KEY)
        .execute(&pool)
        .await
        .unwrap();

        insert_dispute(&pool, dispute_id, order_id, "initiated").await;

        let result = crate::db::find_user_disputes_by_master_key(&pool, VALID_MASTER_KEY)
            .await
            .unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].dispute_id, dispute_id);
        assert_eq!(result[0].order_id, order_id);
        assert_eq!(result[0].trade_index, 5);
    }

    #[tokio::test]
    async fn finds_in_progress_dispute_for_seller() {
        let pool = setup_restore_db().await;
        let order_id = uuid::Uuid::new_v4();
        let dispute_id = uuid::Uuid::new_v4();

        sqlx::query(
            r#"INSERT INTO orders (id, kind, event_id, status, premium, payment_method,
                    amount, fiat_code, fiat_amount, created_at, expires_at,
                    failed_payment, payment_attempts, dev_fee, dev_fee_paid,
                    master_seller_pubkey, trade_index_seller, buyer_dispute, seller_dispute)
            VALUES (?1, 'sell', 'ev1', 'dispute', 0, 'lightning',
                    100000, 'USD', 100, 1700000000, 1700086400,
                    0, 0, 0, 0, ?2, 9, 0, 1)"#,
        )
        .bind(order_id)
        .bind(VALID_MASTER_KEY)
        .execute(&pool)
        .await
        .unwrap();

        insert_dispute(&pool, dispute_id, order_id, "in-progress").await;

        let result = crate::db::find_user_disputes_by_master_key(&pool, VALID_MASTER_KEY)
            .await
            .unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].trade_index, 9);
    }

    #[tokio::test]
    async fn excludes_resolved_disputes() {
        let pool = setup_restore_db().await;
        let order_id = uuid::Uuid::new_v4();

        sqlx::query(
            r#"INSERT INTO orders (id, kind, event_id, status, premium, payment_method,
                    amount, fiat_code, fiat_amount, created_at, expires_at,
                    failed_payment, payment_attempts, dev_fee, dev_fee_paid,
                    master_buyer_pubkey, trade_index_buyer)
            VALUES (?1, 'buy', 'ev1', 'success', 0, 'lightning',
                    100000, 'USD', 100, 1700000000, 1700086400,
                    0, 0, 0, 0, ?2, 1)"#,
        )
        .bind(order_id)
        .bind(VALID_MASTER_KEY)
        .execute(&pool)
        .await
        .unwrap();

        insert_dispute(&pool, uuid::Uuid::new_v4(), order_id, "settled").await;

        let result = crate::db::find_user_disputes_by_master_key(&pool, VALID_MASTER_KEY)
            .await
            .unwrap();
        assert!(
            result.is_empty(),
            "Resolved disputes should not be returned"
        );
    }

    #[tokio::test]
    async fn returns_empty_disputes_when_no_match() {
        let pool = setup_restore_db().await;

        let result = crate::db::find_user_disputes_by_master_key(&pool, VALID_MASTER_KEY)
            .await
            .unwrap();
        assert!(result.is_empty());
    }

    // --- make_unwrapped_gift helper smoke test ---

    #[test]
    fn unwrapped_gift_sender_and_rumor_pubkey_are_set() {
        let gift = make_unwrapped_gift(VALID_MASTER_KEY, VALID_TRADE_KEY_CORE);
        assert_eq!(gift.sender.to_string(), VALID_MASTER_KEY);
        assert_eq!(gift.rumor.pubkey.to_string(), VALID_TRADE_KEY_CORE);
    }
}
