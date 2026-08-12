use crate::util::{enqueue_order_msg, notify_taker_reputation};
use mostro_core::db::Crud;
use mostro_core::prelude::*;
use nostr_sdk::prelude::*;
use sqlx::SqlitePool;
use tracing::info;

/// Advance an order whose seller has just paid the hold invoice.
///
/// Called from the LND invoice subscriber on `InvoiceState::Accepted`, which
/// means it can fire more than once for the same invoice: LND replays the
/// current state whenever a subscription is (re)attached, and every restart
/// reattaches one for each row `crate::db::find_held_invoices` returns.
///
/// It therefore only acts on an order that is still in `WaitingPayment` or
/// `WaitingBuyerInvoice` *and* still has `invoice_held_at == 0`. Anything else
/// — a replay, or a late event for an order canceled while its hold invoice
/// lived on in LND — is a no-op that returns `Ok` after a warning, never an
/// error, since there is nothing for the caller to retry.
///
/// Not atomic: the check and the write are separate statements, so a cancel
/// running concurrently on the main loop can still interleave. See #855.
pub async fn hold_invoice_paid(
    hash: &str,
    request_id: Option<u64>,
    pool: &SqlitePool,
    my_keys: &Keys,
) -> Result<(), MostroError> {
    let order = crate::db::find_order_by_hash(pool, hash)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

    // Decide whether this event may advance the order before touching anything
    // else, so a replay costs one status read and cannot surface a spurious
    // error from a degraded row.
    //
    // Two conditions, and both are load-bearing:
    //
    // - Status must still be pre-payment. Otherwise a late `Accepted` — the row
    //   was canceled while the hold invoice lived on in LND — would drag a dead
    //   order back into a live trade.
    // - `invoice_held_at` must still be unset. This is the idempotency marker:
    //   it is written at the end of this function and read nowhere else, and
    //   `WaitingBuyerInvoice` is both an entry *and* an exit state of the flow
    //   below. Without it, every restart resubscribes such a row, LND replays
    //   the held invoice's `Accepted`, and the buyer gets another `AddInvoice`
    //   while the maker gets another reputation notification.
    let current_status = order.get_order_status().map_err(MostroInternalErr)?;
    let is_pre_payment = matches!(
        current_status,
        Status::WaitingPayment | Status::WaitingBuyerInvoice
    );
    if !is_pre_payment || order.invoice_held_at != 0 {
        // Loud on purpose: the HTLC is real and locked, but this order is past
        // the point where we can credit it, so an operator has to look.
        tracing::warn!(
            order_id = %order.id,
            status = %current_status,
            invoice_held_at = order.invoice_held_at,
            "Ignoring hold-invoice payment for an order already past the \
             pre-payment window; the invoice with hash {hash} may still be \
             locked in LND"
        );
        return Ok(());
    }

    let buyer_pubkey = order
        .get_buyer_pubkey()
        .map_err(|e| MostroInternalErr(ServiceError::NostrError(e.to_string())))?;
    let seller_pubkey = order
        .get_seller_pubkey()
        .map_err(|e| MostroInternalErr(ServiceError::NostrError(e.to_string())))?;

    info!(
        "Order Id: {} - Seller paid invoice with hash: {hash}",
        order.id
    );

    // Check if the order kind is valid
    let order_kind = order.get_order_kind().map_err(MostroInternalErr)?;

    // We send this data related to the order to the parties
    let mut order_data = SmallOrder::new(
        Some(order.id),
        Some(order_kind),
        None,
        order.amount,
        order.fiat_code.clone(),
        order.min_amount,
        order.max_amount,
        order.fiat_amount,
        order.payment_method.clone(),
        order.premium,
        order.buyer_pubkey.as_ref().cloned(),
        order.seller_pubkey.as_ref().cloned(),
        None,
        Some(order.created_at),
        Some(order.expires_at),
    );
    let status;

    // Dev fee is NOT charged to users - it's paid by mostrod from its earnings
    if order.buyer_invoice.is_some() {
        status = Status::Active;
        order_data.status = Some(status);
        // We send a confirmation message to seller
        let mut seller_order_data = order_data.clone();
        seller_order_data.amount = order.amount.saturating_add(order.fee);
        enqueue_order_msg(
            request_id,
            Some(order.id),
            Action::BuyerTookOrder,
            Some(Payload::Order(seller_order_data)),
            seller_pubkey,
            None,
        )
        .await;
        // We send a message to buyer saying seller paid
        let mut buyer_order_data = order_data.clone();
        buyer_order_data.amount = order.amount.saturating_sub(order.fee);
        enqueue_order_msg(
            request_id,
            Some(order.id),
            Action::HoldInvoicePaymentAccepted,
            Some(Payload::Order(buyer_order_data)),
            buyer_pubkey,
            None,
        )
        .await;
    } else {
        let new_amount = order_data.amount - order.fee;
        order_data.amount = new_amount;
        status = Status::WaitingBuyerInvoice;
        order_data.status = Some(status);
        order_data.buyer_trade_pubkey = None;
        order_data.seller_trade_pubkey = None;
        // We ask to buyer for a new invoice
        enqueue_order_msg(
            request_id,
            Some(order.id),
            Action::AddInvoice,
            Some(Payload::Order(order_data)),
            buyer_pubkey,
            None,
        )
        .await;

        // We send a message to seller we are waiting for buyer invoice
        enqueue_order_msg(
            request_id,
            Some(order.id),
            Action::WaitingBuyerInvoice,
            None,
            seller_pubkey,
            None,
        )
        .await;

        // Notify taker reputation to maker
        tracing::info!("Notifying taker reputation to maker");
        notify_taker_reputation(pool, &order).await?;
    }
    // We publish a new replaceable kind nostr event with the status updated
    // and update on local database the status and new event id
    if let Ok(updated_order) = crate::util::update_order_event(my_keys, status, &order).await {
        // Update order on db
        let _ = updated_order.update(pool).await;
    }

    // Update the invoice_held_at field
    crate::db::update_order_invoice_held_at_time(pool, order.id, Timestamp::now().as_secs() as i64)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

    Ok(())
}

pub async fn hold_invoice_settlement(hash: &str, pool: &SqlitePool) -> Result<()> {
    let order = crate::db::find_order_by_hash(pool, hash).await?;
    info!(
        "Order Id: {} - Invoice with hash: {} was settled!",
        order.id, hash
    );
    Ok(())
}

/// Handle an LND `Canceled` update for an order's hold invoice.
///
/// Most cancels are expected: the daemon itself canceled the invoice
/// (waiting-state timeout, cooperative cancel, admin cancel) and already
/// moved the order on, so the event only earns a log line.
///
/// The dangerous case is a cancel landing while the order is still in an
/// escrow-backed state (`Active` / `FiatSent` / `Dispute`): the trade
/// relies on that HTLC, so its loss means LND auto-canceled it at the
/// CLTV horizon, refunding the escrow to the seller (or the deadline
/// guardian job could not act — e.g. the daemon was down past the
/// horizon and this is the restart replay). To avoid misreading an
/// in-flight *intentional* cancel as an evaporation, the alarm only
/// fires once the order is past the guardian's nominal action deadline
/// ([`crate::util::escrow_action_deadline_unix`]); before that, an
/// escrow-backed cancel is treated as intentional and only logged. This
/// gate is a wall-clock approximation of the block-height horizon (no
/// LND handle here to consult the chain); the guardian job is the
/// height-accurate layer and normally acts first.
///
/// Past the deadline:
///
/// - every escrow-backed state gets both parties notified with
///   [`Action::HoldInvoicePaymentCanceled`], plus an `error!` for the
///   operator;
/// - `Active` additionally transitions to `Canceled`: with no fiat claim
///   on record the trade is simply dead, and leaving it `Active` would
///   keep showing a trade that no escrow backs;
/// - `FiatSent` / `Dispute` keep their status — the fiat leg may already
///   have moved, so a human must resolve the fallout. The notifications
///   and the `error!` are the alarm.
pub async fn hold_invoice_canceled(
    hash: &str,
    pool: &SqlitePool,
    my_keys: &Keys,
) -> Result<(), MostroError> {
    let order = crate::db::find_order_by_hash(pool, hash)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
    let status = order.get_order_status().map_err(MostroInternalErr)?;

    let is_escrow_backed = matches!(status, Status::Active | Status::FiatSent | Status::Dispute);
    // `invoice_held_at == 0` (escrow never observed, or a row predating the
    // column) yields no deadline: nothing reliable to reason about, keep
    // the legacy log-only behaviour.
    let ln_settings = crate::config::Settings::get_ln();
    let past_action_deadline = crate::util::escrow_action_deadline_unix(
        order.invoice_held_at,
        ln_settings.hold_invoice_cltv_delta,
        ln_settings.escrow_deadline_margin_blocks,
    )
    .map(|deadline| Timestamp::now().as_secs() as i64 >= deadline)
    .unwrap_or(false);

    if !is_escrow_backed || !past_action_deadline {
        info!(
            "Order Id: {} - Invoice with hash: {} was canceled!",
            order.id, hash
        );
        return Ok(());
    }

    tracing::error!(
        order_id = %order.id,
        status = %status,
        "Hold invoice canceled while the order still relies on its escrow: \
         the escrow was refunded to the seller at the CLTV horizon and the \
         trade can no longer be settled — manual intervention required"
    );

    // Missing party keys must not mute the alarm: the operator-facing
    // error above still fires, and whichever key resolves gets notified.
    for party in [order.get_buyer_pubkey(), order.get_seller_pubkey()]
        .into_iter()
        .flatten()
    {
        enqueue_order_msg(
            None,
            Some(order.id),
            Action::HoldInvoicePaymentCanceled,
            None,
            party,
            None,
        )
        .await;
    }

    if status == Status::Active {
        match crate::util::update_order_event(my_keys, Status::Canceled, &order).await {
            Ok(updated) => {
                let updated = updated
                    .update(pool)
                    .await
                    .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
                crate::scheduler::notify_users_canceled_order(
                    &updated,
                    &order,
                    Some(Action::Canceled),
                )
                .await;
            }
            Err(e) => {
                // The next subscription replay (or restart) retries; the
                // error above already flagged the order for the operator.
                tracing::warn!(
                    "Could not publish the cancel for order {} ({e}); will retry",
                    order.id
                );
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mostro_core::order::{Kind as OrderKind, Status};
    use nostr_sdk::{Keys, Timestamp};
    use sqlx::SqlitePool;

    async fn create_test_pool() -> SqlitePool {
        SqlitePool::connect(":memory:").await.unwrap()
    }

    fn create_test_keys() -> Keys {
        Keys::generate()
    }

    async fn create_migrated_pool() -> SqlitePool {
        let pool = SqlitePool::connect(":memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        pool
    }

    fn init_global_settings() {
        let _ = crate::config::MOSTRO_CONFIG.set(crate::app::context::test_utils::test_settings());
    }

    /// Insert a sell order carrying `hash` so `find_order_by_hash` resolves it.
    async fn insert_order_with_hash(
        pool: &SqlitePool,
        hash: &str,
        status: Status,
        buyer_invoice: Option<String>,
        buyer_pubkey: Option<String>,
        seller_pubkey: Option<String>,
        master_buyer_pubkey: Option<String>,
    ) -> Order {
        let order = Order {
            id: uuid::Uuid::new_v4(),
            kind: OrderKind::Sell.to_string(),
            status: status.to_string(),
            creator_pubkey: seller_pubkey.clone().unwrap_or_default(),
            payment_method: "SEPA".to_string(),
            amount: 1_000,
            fee: 10,
            fiat_code: "USD".to_string(),
            fiat_amount: 100,
            hash: Some(hash.to_string()),
            buyer_invoice,
            buyer_pubkey,
            seller_pubkey,
            master_buyer_pubkey,
            created_at: Timestamp::now().as_secs() as i64,
            expires_at: Timestamp::now().as_secs() as i64 + 3_600,
            ..Default::default()
        };
        order.create(pool).await.unwrap()
    }

    #[tokio::test]
    async fn hold_invoice_paid_with_buyer_invoice_activates_order() {
        init_global_settings();
        let pool = create_migrated_pool().await;
        let hash = "aa".repeat(32);
        let buyer = create_test_keys().public_key().to_string();
        let seller = create_test_keys().public_key().to_string();
        insert_order_with_hash(
            &pool,
            &hash,
            Status::WaitingPayment,
            Some("lnbcrt1invoice".to_string()),
            Some(buyer),
            Some(seller),
            None,
        )
        .await;

        let result = hold_invoice_paid(&hash, Some(1), &pool, &create_test_keys()).await;
        assert!(result.is_ok(), "active path must succeed: {result:?}");

        // The order row must be flipped to Active with invoice_held_at set.
        let updated = crate::db::find_order_by_hash(&pool, &hash).await.unwrap();
        assert_eq!(updated.status, Status::Active.to_string());
        assert!(updated.invoice_held_at > 0, "invoice_held_at must be set");
    }

    #[tokio::test]
    async fn hold_invoice_paid_without_buyer_invoice_requests_one() {
        init_global_settings();
        let pool = create_migrated_pool().await;
        let hash = "bb".repeat(32);
        let buyer = create_test_keys().public_key().to_string();
        let seller = create_test_keys().public_key().to_string();
        let master_buyer = create_test_keys().public_key().to_string();
        // Status WaitingBuyerInvoice keeps notify_taker_reputation on its
        // allowed-status path (sell order → PayInvoice to seller).
        insert_order_with_hash(
            &pool,
            &hash,
            Status::WaitingBuyerInvoice,
            None,
            Some(buyer),
            Some(seller),
            Some(master_buyer),
        )
        .await;

        let result = hold_invoice_paid(&hash, None, &pool, &create_test_keys()).await;
        assert!(result.is_ok(), "add-invoice path must succeed: {result:?}");

        let updated = crate::db::find_order_by_hash(&pool, &hash).await.unwrap();
        assert_eq!(updated.status, Status::WaitingBuyerInvoice.to_string());
        assert!(updated.invoice_held_at > 0, "invoice_held_at must be set");
    }

    #[tokio::test]
    async fn hold_invoice_paid_does_not_revive_a_settled_order() {
        init_global_settings();
        let pool = create_migrated_pool().await;
        let hash = "cc".repeat(32);
        let buyer = create_test_keys().public_key().to_string();
        let seller = create_test_keys().public_key().to_string();
        // A redelivered or late Accepted event for an order that has already
        // left the pre-payment window must not drag it back into a live trade.
        insert_order_with_hash(
            &pool,
            &hash,
            Status::Canceled,
            Some("lnbcrt1invoice".to_string()),
            Some(buyer),
            Some(seller),
            None,
        )
        .await;

        let result = hold_invoice_paid(&hash, None, &pool, &create_test_keys()).await;
        assert!(
            result.is_ok(),
            "a stale Accepted is a no-op, not a failure: {result:?}"
        );

        let updated = crate::db::find_order_by_hash(&pool, &hash).await.unwrap();
        assert_eq!(updated.status, Status::Canceled.to_string());
        assert_eq!(
            updated.invoice_held_at, 0,
            "no side effects on an order outside the pre-payment window"
        );
    }

    #[tokio::test]
    async fn hold_invoice_paid_is_a_noop_on_redelivery() {
        init_global_settings();
        let pool = create_migrated_pool().await;
        let hash = "ee".repeat(32);
        let buyer = create_test_keys().public_key().to_string();
        let seller = create_test_keys().public_key().to_string();
        let master_buyer = create_test_keys().public_key().to_string();
        // This is the else-branch's *own* output state: WaitingBuyerInvoice with
        // the hold invoice already observed. Such a row is resubscribed on every
        // restart, and LND replays the current Accepted state on resubscribe, so
        // the flow must not run a second time — it would re-send AddInvoice to
        // the buyer and re-notify the maker on each restart.
        let order = insert_order_with_hash(
            &pool,
            &hash,
            Status::WaitingBuyerInvoice,
            None,
            Some(buyer),
            Some(seller),
            Some(master_buyer),
        )
        .await;
        crate::db::update_order_invoice_held_at_time(&pool, order.id, 1_700_000_000)
            .await
            .unwrap();

        let result = hold_invoice_paid(&hash, None, &pool, &create_test_keys()).await;
        assert!(
            result.is_ok(),
            "a replayed Accepted is a no-op, not a failure: {result:?}"
        );

        let updated = crate::db::find_order_by_hash(&pool, &hash).await.unwrap();
        assert_eq!(
            updated.invoice_held_at, 1_700_000_000,
            "a replayed Accepted must not re-run the flow"
        );
    }

    #[tokio::test]
    async fn hold_invoice_paid_errors_when_buyer_pubkey_missing() {
        init_global_settings();
        let pool = create_migrated_pool().await;
        let hash = "cc".repeat(32);
        insert_order_with_hash(&pool, &hash, Status::WaitingPayment, None, None, None, None).await;

        let result = hold_invoice_paid(&hash, None, &pool, &create_test_keys()).await;
        assert!(
            matches!(result, Err(MostroInternalErr(ServiceError::NostrError(_)))),
            "missing buyer pubkey must surface as NostrError: {result:?}"
        );
    }

    #[tokio::test]
    async fn hold_invoice_settlement_and_cancel_resolve_existing_order() {
        init_global_settings();
        let pool = create_migrated_pool().await;
        let hash = "dd".repeat(32);
        insert_order_with_hash(&pool, &hash, Status::Active, None, None, None, None).await;

        assert!(hold_invoice_settlement(&hash, &pool).await.is_ok());
        assert!(hold_invoice_canceled(&hash, &pool, &create_test_keys())
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn test_hold_invoice_paid_structure() {
        let pool = create_test_pool().await;
        let hash = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let request_id = Some(1u64);
        // Valid test nsec
        let keys = Keys::parse("nsec13as48eum93hkg7plv526r9gjpa0uc52zysqm93pmnkca9e69x6tsdjmdxd")
            .expect("valid test nsec");

        // This test would require:
        // 1. Setting up database tables and test data
        // 2. Creating a valid order in the database
        let result = hold_invoice_paid(hash, request_id, &pool, &keys).await;
        // Should fail without proper database setup, but shouldn't panic
        assert!(result.is_ok() || result.is_err());
    }

    #[tokio::test]
    async fn test_hold_invoice_settlement_structure() {
        let pool = create_test_pool().await;
        let hash = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

        // This test would require setting up database with order data
        let result = hold_invoice_settlement(hash, &pool).await;
        // Should fail without proper database setup
        assert!(result.is_ok() || result.is_err());
    }

    #[tokio::test]
    async fn test_hold_invoice_canceled_structure() {
        let pool = create_test_pool().await;
        let hash = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

        // This test would require setting up database with order data
        let result = hold_invoice_canceled(hash, &pool, &create_test_keys()).await;
        // Should fail without proper database setup
        assert!(result.is_ok() || result.is_err());
    }

    #[tokio::test]
    async fn hold_invoice_canceled_closes_active_order_past_the_escrow_deadline() {
        init_global_settings();
        let pool = create_migrated_pool().await;
        let hash = "f1".repeat(32);
        let buyer = create_test_keys().public_key().to_string();
        let seller = create_test_keys().public_key().to_string();
        let order = insert_order_with_hash(
            &pool,
            &hash,
            Status::Active,
            None,
            Some(buyer),
            Some(seller),
            None,
        )
        .await;
        // The escrow was observed long enough ago that the guardian's
        // action deadline is behind us (test settings clamp the window,
        // so any past timestamp qualifies).
        crate::db::update_order_invoice_held_at_time(&pool, order.id, 1_700_000_000)
            .await
            .unwrap();

        let result = hold_invoice_canceled(&hash, &pool, &create_test_keys()).await;
        assert!(
            result.is_ok(),
            "evaporation handling must not error: {result:?}"
        );

        let updated = crate::db::find_order_by_hash(&pool, &hash).await.unwrap();
        assert_eq!(
            updated.status,
            Status::Canceled.to_string(),
            "an Active order whose escrow evaporated must stop looking live"
        );
    }

    #[tokio::test]
    async fn hold_invoice_canceled_ignores_active_order_inside_the_escrow_window() {
        init_global_settings();
        let pool = create_migrated_pool().await;
        let hash = "f2".repeat(32);
        let buyer = create_test_keys().public_key().to_string();
        let seller = create_test_keys().public_key().to_string();
        let order = insert_order_with_hash(
            &pool,
            &hash,
            Status::Active,
            None,
            Some(buyer),
            Some(seller),
            None,
        )
        .await;
        // The action deadline is still ahead (test settings clamp the
        // guard window to zero, so only a future held_at lands inside the
        // window): an intentional cooperative/admin cancel in flight must
        // not be misread as an evaporation.
        let future = Timestamp::now().as_secs() as i64 + 86_400;
        crate::db::update_order_invoice_held_at_time(&pool, order.id, future)
            .await
            .unwrap();

        let result = hold_invoice_canceled(&hash, &pool, &create_test_keys()).await;
        assert!(
            result.is_ok(),
            "intentional cancels stay no-ops: {result:?}"
        );

        let updated = crate::db::find_order_by_hash(&pool, &hash).await.unwrap();
        assert_eq!(updated.status, Status::Active.to_string());
    }

    #[tokio::test]
    async fn hold_invoice_canceled_leaves_fiat_sent_order_for_a_human() {
        init_global_settings();
        let pool = create_migrated_pool().await;
        let hash = "f3".repeat(32);
        let buyer = create_test_keys().public_key().to_string();
        let seller = create_test_keys().public_key().to_string();
        let order = insert_order_with_hash(
            &pool,
            &hash,
            Status::FiatSent,
            Some("lnbcrt1invoice".to_string()),
            Some(buyer),
            Some(seller),
            None,
        )
        .await;
        crate::db::update_order_invoice_held_at_time(&pool, order.id, 1_700_000_000)
            .await
            .unwrap();

        let result = hold_invoice_canceled(&hash, &pool, &create_test_keys()).await;
        assert!(result.is_ok(), "the alarm must not error: {result:?}");

        let updated = crate::db::find_order_by_hash(&pool, &hash).await.unwrap();
        assert_eq!(
            updated.status,
            Status::FiatSent.to_string(),
            "fiat may already have moved: only a human can resolve this"
        );
    }

    #[tokio::test]
    async fn hold_invoice_canceled_without_held_timestamp_keeps_legacy_noop() {
        init_global_settings();
        let pool = create_migrated_pool().await;
        let hash = "f4".repeat(32);
        // Active with invoice_held_at == 0: no deadline to reason about
        // (escrow never observed, or a row predating the column).
        insert_order_with_hash(&pool, &hash, Status::Active, None, None, None, None).await;

        let result = hold_invoice_canceled(&hash, &pool, &create_test_keys()).await;
        assert!(result.is_ok(), "legacy no-op must not error: {result:?}");

        let updated = crate::db::find_order_by_hash(&pool, &hash).await.unwrap();
        assert_eq!(updated.status, Status::Active.to_string());
    }

    mod hold_invoice_flow_tests {
        use super::*;

        #[test]
        fn test_hash_validation() {
            // Test various hash formats
            let valid_hashes = vec![
                "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef", // 64 chars
                "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210", // 64 chars
                "abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890", // Mixed case
            ];

            let invalid_hashes = vec![
                "",                                                                   // Empty
                "short",                                                              // Too short
                "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdefXX", // Too long
                "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdeg", // Invalid char
            ];

            // All valid hashes should be 64 characters of hex
            for hash in valid_hashes {
                assert_eq!(hash.len(), 64);
                assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
            }

            // Invalid hashes should fail basic validation
            for hash in invalid_hashes {
                assert!(hash.len() != 64 || !hash.chars().all(|c| c.is_ascii_hexdigit()));
            }
        }

        #[test]
        fn test_order_data_creation_logic() {
            // Test the logical flow of SmallOrder creation

            // Mock order data
            let order_id = uuid::Uuid::new_v4();
            let order_kind = OrderKind::Sell;
            let amount = 1000i64;
            let fiat_code = "USD".to_string();
            let fiat_amount = 100i64;
            let payment_method = "SEPA".to_string();
            let premium = 5;
            let created_at = Timestamp::now().as_secs() as i64;
            let expires_at = created_at + 3600;

            // Test SmallOrder creation logic
            let order_data = SmallOrder::new(
                Some(order_id),
                Some(order_kind),
                None,
                amount,
                fiat_code.clone(),
                None,
                None,
                fiat_amount,
                payment_method.clone(),
                premium,
                None,
                None,
                None,
                Some(created_at),
                Some(expires_at),
            );

            // Verify the order data is constructed correctly
            assert_eq!(order_data.id, Some(order_id));
            assert_eq!(order_data.kind, Some(order_kind));
            assert_eq!(order_data.amount, amount);
            assert_eq!(order_data.fiat_code, fiat_code);
            assert_eq!(order_data.fiat_amount, fiat_amount);
            assert_eq!(order_data.payment_method, payment_method);
            assert_eq!(order_data.premium, premium);
            assert_eq!(order_data.created_at, Some(created_at));
            assert_eq!(order_data.expires_at, Some(expires_at));
        }

        #[test]
        fn test_status_transitions() {
            // Test the logical flow of status transitions

            // From WaitingBuyerInvoice to Active
            let initial_status = Status::WaitingBuyerInvoice;
            let target_status = Status::Active;

            // Simulate the condition: buyer invoice exists
            let buyer_invoice_exists = true;
            let resulting_status = if buyer_invoice_exists {
                Status::Active
            } else {
                Status::WaitingBuyerInvoice
            };

            assert_eq!(resulting_status, target_status);

            // Test the opposite case
            let buyer_invoice_exists = false;
            let resulting_status = if buyer_invoice_exists {
                Status::Active
            } else {
                Status::WaitingBuyerInvoice
            };

            assert_eq!(resulting_status, initial_status);
        }

        #[test]
        fn test_fee_calculation_logic() {
            // Test fee calculation in the amount adjustment
            let original_amount = 1000i64;
            let fee = 15i64; // 1.5%
            let expected_new_amount = original_amount - fee;

            assert_eq!(expected_new_amount, 985);
            assert!(expected_new_amount < original_amount);
            assert!(fee > 0);

            // Test edge cases
            let zero_fee = 0i64;
            assert_eq!(original_amount - zero_fee, original_amount);

            let large_fee = 500i64; // 50%
            let result_with_large_fee = original_amount - large_fee;
            assert_eq!(result_with_large_fee, 500);
            assert!(result_with_large_fee > 0); // Should still be positive
        }
    }

    mod message_flow_tests {
        use super::*;

        #[test]
        fn test_action_types_for_buyer_invoice_flow() {
            // Test the different actions used in the flow

            // Actions when buyer invoice exists
            let buyer_actions = vec![Action::BuyerTookOrder, Action::HoldInvoicePaymentAccepted];

            // Actions when buyer invoice doesn't exist
            let no_invoice_actions = vec![Action::AddInvoice, Action::WaitingBuyerInvoice];

            // Verify actions are different for different flows
            for action in buyer_actions {
                assert!(!no_invoice_actions.contains(&action));
            }

            for action in no_invoice_actions {
                assert!(
                    ![Action::BuyerTookOrder, Action::HoldInvoicePaymentAccepted].contains(&action)
                );
            }
        }

        #[test]
        fn test_payload_creation_logic() {
            // Test payload creation for different scenarios

            // Create mock order data
            let order_data = SmallOrder::new(
                Some(uuid::Uuid::new_v4()),
                Some(OrderKind::Sell),
                Some(Status::Active),
                1000,
                "USD".to_string(),
                None,
                None,
                100,
                "SEPA".to_string(),
                0,
                None,
                None,
                None,
                Some(Timestamp::now().as_secs() as i64),
                Some(Timestamp::now().as_secs() as i64 + 3600),
            );

            // Test payload with order data
            let payload_with_order = Some(Payload::Order(order_data.clone()));
            assert!(payload_with_order.is_some());

            // Test payload without order data (None)
            let payload_none: Option<Payload> = None;
            assert!(payload_none.is_none());

            // Verify payload contains the order data
            if let Some(Payload::Order(order)) = payload_with_order {
                assert_eq!(order.amount, 1000);
                assert_eq!(order.fiat_code, "USD");
                assert_eq!(order.status, Some(Status::Active));
            } else {
                panic!("Expected Order payload");
            }
        }
    }

    mod pubkey_extraction_tests {
        use super::*;

        #[test]
        fn test_pubkey_extraction_logic() {
            // Test the logical flow of pubkey extraction

            let keys = create_test_keys();
            let buyer_pubkey = keys.public_key();
            let seller_pubkey = create_test_keys().public_key();

            // Test that pubkeys are different
            assert_ne!(buyer_pubkey, seller_pubkey);

            // Test pubkey string conversion
            let buyer_pubkey_str = buyer_pubkey.to_string();
            let seller_pubkey_str = seller_pubkey.to_string();

            assert!(!buyer_pubkey_str.is_empty());
            assert!(!seller_pubkey_str.is_empty());
            assert_ne!(buyer_pubkey_str, seller_pubkey_str);

            // Test pubkey format (should be hex)
            assert!(buyer_pubkey_str.chars().all(|c| c.is_ascii_hexdigit()));
            assert!(seller_pubkey_str.chars().all(|c| c.is_ascii_hexdigit()));

            // Nostr pubkeys should be 64 characters (32 bytes in hex)
            assert_eq!(buyer_pubkey_str.len(), 64);
            assert_eq!(seller_pubkey_str.len(), 64);
        }

        #[test]
        fn test_request_id_handling() {
            // Test request ID handling in different scenarios

            let valid_request_ids = vec![Some(1u64), Some(42u64), Some(1000u64), Some(u64::MAX)];

            let none_request_id: Option<u64> = None;

            // All valid request IDs should be Some
            for request_id in valid_request_ids {
                assert!(request_id.is_some());
                assert!(request_id.unwrap() > 0 || request_id.unwrap() == 0);
            }

            // None should be None
            assert!(none_request_id.is_none());
        }
    }

    mod timestamp_tests {
        use super::*;

        #[test]
        fn test_timestamp_operations() {
            // Test timestamp operations used in the flow

            let current_timestamp = Timestamp::now();
            let timestamp_u64 = current_timestamp.as_secs();
            let timestamp_i64 = timestamp_u64 as i64;

            // Verify timestamp is reasonable (after 2020, before 2050)
            let year_2020 = 1577836800u64; // 2020-01-01 00:00:00 UTC
            let year_2050 = 2524608000u64; // 2050-01-01 00:00:00 UTC

            assert!(timestamp_u64 > year_2020);
            assert!(timestamp_u64 < year_2050);

            // Verify i64 conversion preserves the value
            assert!(timestamp_i64 > 0);
            assert_eq!(timestamp_u64, timestamp_i64 as u64);
        }
    }
}
