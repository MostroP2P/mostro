//! `payment_success` wiring for payer history
//! (`docs/PAYER_HISTORY_ANTI_TRIANGULATION.md` §10.5, §14). Loaded from
//! `release.rs` via `#[path]` so it can reach the private `payment_success`.

use super::*;
use crate::app::payer::db::{
    find_declaration, load_history, prune_declarations_for_terminal_orders, upsert_declaration,
};
use crate::app::payer::test_support::{
    create_test_pool, ctx_with, hash, order_in, payer_settings, queued_for, Parties,
};
use crate::config::MOSTRO_CONFIG;
use crate::util::{orderbook_publish_attempts, ORDERBOOK_QUEUE_TEST_LOCK};
use sqlx::SqlitePool;

fn init_global_config() {
    let _ = MOSTRO_CONFIG.set(crate::app::context::test_utils::test_settings());
}

async fn success_at(pool: &SqlitePool, order_id: uuid::Uuid) -> Option<i64> {
    sqlx::query_scalar("SELECT success_at FROM orders WHERE id = ?1")
        .bind(order_id)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn status(pool: &SqlitePool, order_id: uuid::Uuid) -> String {
    Order::by_id(pool, order_id).await.unwrap().unwrap().status
}

async fn payer_rows(pool: &SqlitePool) -> i64 {
    sqlx::query_scalar(
        "SELECT (SELECT COUNT(*) FROM payer_history) \
              + (SELECT COUNT(*) FROM payer_history_counterparties)",
    )
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn finalize(ctx: &AppContext, order: &mut Order, parties: Parties) -> Result<bool> {
    payment_success(ctx, order, parties.buyer, &Keys::generate(), None).await
}

async fn notified(order_id: uuid::Uuid) -> (bool, bool) {
    let actions: Vec<Action> = queued_for(order_id)
        .await
        .into_iter()
        .map(|q| q.action)
        .collect();
    (
        actions.contains(&Action::PurchaseCompleted),
        actions.contains(&Action::Rate),
    )
}

#[tokio::test]
async fn feature_off_still_stamps_success_at_and_writes_no_payer_rows() {
    // D-10, item 2: the stamp is unconditional; everything else is gated.
    init_global_config();
    let pool = create_test_pool().await;
    let ctx = ctx_with(&pool, None);
    let parties = Parties::reputation();
    let mut order = order_in(&pool, Status::SettledHoldInvoice, parties).await;
    upsert_declaration(&pool, order.id, &hash('a'), 1)
        .await
        .unwrap();

    assert!(finalize(&ctx, &mut order, parties).await.unwrap());

    assert_eq!(status(&pool, order.id).await, Status::Success.to_string());
    assert!(success_at(&pool, order.id).await.is_some());
    assert_eq!(payer_rows(&pool).await, 0);
    // The leftover declaration is not consumed; the prune job, which runs
    // with the feature off too, removes it without recording anything.
    assert!(find_declaration(&pool, order.id).await.unwrap().is_some());
    assert_eq!(notified(order.id).await, (true, true));
    assert_eq!(
        prune_declarations_for_terminal_orders(&pool).await.unwrap(),
        1
    );
    assert!(find_declaration(&pool, order.id).await.unwrap().is_none());
    assert_eq!(payer_rows(&pool).await, 0);
}

#[tokio::test]
async fn feature_on_records_history_in_the_success_transaction() {
    init_global_config();
    let _queue = ORDERBOOK_QUEUE_TEST_LOCK.lock().await;
    let pool = create_test_pool().await;
    let ctx = ctx_with(&pool, Some(payer_settings(true, false)));
    let parties = Parties::reputation();
    let mut order = order_in(&pool, Status::SettledHoldInvoice, parties).await;
    upsert_declaration(&pool, order.id, &hash('a'), 1)
        .await
        .unwrap();

    assert!(finalize(&ctx, &mut order, parties).await.unwrap());

    assert_eq!(status(&pool, order.id).await, Status::Success.to_string());
    let stamped = success_at(&pool, order.id).await.expect("stamped");
    let h = load_history(&pool, &parties.buyer_master.to_string(), &hash('a'))
        .await
        .unwrap();
    assert_eq!((h.successful_trades, h.distinct_counterparties), (1, 1));
    assert_eq!(h.last_success_at, Some(stamped));
    assert!(
        find_declaration(&pool, order.id).await.unwrap().is_none(),
        "consumed"
    );
    assert_eq!(notified(order.id).await, (true, true));
    // Published after the commit. With no relay client in tests the publish
    // fails and queues the order; arming the queue before the commit must
    // not have consumed an attempt of its own.
    assert_eq!(orderbook_publish_attempts(order.id), Some(1));
}

#[tokio::test]
async fn a_failing_history_write_leaves_no_visible_success() {
    // §10.5 invariant: no failure before the commit produces an externally
    // visible Success — not on the relays, not in the message queue.
    init_global_config();
    let _queue = ORDERBOOK_QUEUE_TEST_LOCK.lock().await;
    let pool = create_test_pool().await;
    let ctx = ctx_with(&pool, Some(payer_settings(true, false)));
    let parties = Parties::reputation();
    let mut order = order_in(&pool, Status::SettledHoldInvoice, parties).await;
    // A row bump_history refuses (not 64 lowercase hex): the hook fails.
    upsert_declaration(&pool, order.id, "NOT-A-HASH", 1)
        .await
        .unwrap();

    let finalized = finalize(&ctx, &mut order, parties).await.unwrap_or(false);

    assert!(!finalized, "the caller keeps its marker and retries");
    assert_eq!(
        status(&pool, order.id).await,
        Status::SettledHoldInvoice.to_string()
    );
    assert_eq!(success_at(&pool, order.id).await, None);
    assert_eq!(payer_rows(&pool).await, 0);
    assert!(
        find_declaration(&pool, order.id).await.unwrap().is_some(),
        "still retryable"
    );
    assert_eq!(notified(order.id).await, (false, false));
    assert_eq!(
        orderbook_publish_attempts(order.id),
        None,
        "nothing published or armed"
    );
}

#[tokio::test]
async fn an_already_finalized_order_records_nothing_and_keeps_its_stamp() {
    init_global_config();
    let pool = create_test_pool().await;
    let ctx = ctx_with(&pool, Some(payer_settings(true, false)));
    let parties = Parties::reputation();
    let mut order = order_in(&pool, Status::Success, parties).await;
    sqlx::query("UPDATE orders SET success_at = 123 WHERE id = ?1")
        .bind(order.id)
        .execute(&pool)
        .await
        .unwrap();
    upsert_declaration(&pool, order.id, &hash('a'), 1)
        .await
        .unwrap();

    assert!(
        finalize(&ctx, &mut order, parties).await.unwrap(),
        "terminal"
    );

    assert_eq!(success_at(&pool, order.id).await, Some(123));
    assert_eq!(payer_rows(&pool).await, 0);
    assert_eq!(notified(order.id).await, (false, false));
}

#[tokio::test]
async fn a_second_finalization_does_not_move_the_stamp() {
    init_global_config();
    for payer_history in [None, Some(payer_settings(true, false))] {
        let pool = create_test_pool().await;
        let ctx = ctx_with(&pool, payer_history);
        let parties = Parties::reputation();
        let mut order = order_in(&pool, Status::SettledHoldInvoice, parties).await;
        upsert_declaration(&pool, order.id, &hash('a'), 1)
            .await
            .unwrap();

        assert!(finalize(&ctx, &mut order, parties).await.unwrap());
        let first = success_at(&pool, order.id).await;
        sqlx::query("UPDATE orders SET success_at = success_at - 10 WHERE id = ?1")
            .bind(order.id)
            .execute(&pool)
            .await
            .unwrap();
        assert!(finalize(&ctx, &mut order, parties).await.unwrap());

        assert_eq!(success_at(&pool, order.id).await, first.map(|t| t - 10));
        let trades = load_history(&pool, &parties.buyer_master.to_string(), &hash('a'))
            .await
            .unwrap()
            .successful_trades;
        assert!(trades <= 1, "at most one increment per order");
    }
}
