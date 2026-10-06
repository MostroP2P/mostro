//! `payment_success` wiring for payer history
//! (`docs/PAYER_HISTORY_ANTI_TRIANGULATION.md` §10.5, §14). Loaded from
//! `release.rs` via `#[path]` so it can reach the private `payment_success`.

use super::*;
use crate::app::payer::db::{
    find_declaration, load_history, prune_declarations_for_terminal_orders, upsert_declaration,
};
use crate::app::payer::test_support::{
    create_test_pool, ctx_with, hash, node_keys, order_in, payer_settings, queued_for, Parties,
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
    payment_success(ctx, order, parties.buyer, node_keys(), None).await
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
    // Reaches the publish path, which writes the global republish queue.
    let _queue = ORDERBOOK_QUEUE_TEST_LOCK.lock().await;
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
    let h = load_history(
        &pool,
        node_keys(),
        &parties.buyer_master.to_string(),
        &hash('a'),
    )
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

/// Assert that `order` finalized without a history row: Success committed,
/// stamped, published and notified, and the declaration left for the prune
/// job (the savepoint rollback restores it).
async fn assert_finalized_without_history(pool: &SqlitePool, order: &Order) {
    assert_eq!(status(pool, order.id).await, Status::Success.to_string());
    assert!(success_at(pool, order.id).await.is_some());
    assert_eq!(payer_rows(pool).await, 0, "no partial history");
    assert_eq!(notified(order.id).await, (true, true));
    assert_eq!(orderbook_publish_attempts(order.id), Some(1));
    assert_eq!(
        prune_declarations_for_terminal_orders(pool).await.unwrap(),
        1
    );
}

#[tokio::test]
async fn a_failing_history_write_still_finalizes_the_paid_order() {
    // §10.5: the buyer is already paid, so an optional bookkeeping write
    // must not hold the order in settled-hold-invoice. A declaration
    // bump_history refuses (not 64 lowercase hex) fails on every retry.
    init_global_config();
    let _queue = ORDERBOOK_QUEUE_TEST_LOCK.lock().await;
    let pool = create_test_pool().await;
    let ctx = ctx_with(&pool, Some(payer_settings(true, false)));
    let parties = Parties::reputation();
    let mut order = order_in(&pool, Status::SettledHoldInvoice, parties).await;
    upsert_declaration(&pool, order.id, "NOT-A-HASH", 1)
        .await
        .unwrap();

    assert!(finalize(&ctx, &mut order, parties).await.unwrap());

    assert_finalized_without_history(&pool, &order).await;
}

#[tokio::test]
async fn a_history_write_failing_midway_leaves_no_partial_rows() {
    // The payer_history row is written before the counterparty row fails:
    // the savepoint must undo it while the Success commits.
    init_global_config();
    let _queue = ORDERBOOK_QUEUE_TEST_LOCK.lock().await;
    let pool = create_test_pool().await;
    let ctx = ctx_with(&pool, Some(payer_settings(true, false)));
    let parties = Parties::reputation();
    let mut order = order_in(&pool, Status::SettledHoldInvoice, parties).await;
    upsert_declaration(&pool, order.id, &hash('a'), 1)
        .await
        .unwrap();
    sqlx::query(
        "CREATE TRIGGER fail_counterparty BEFORE INSERT ON payer_history_counterparties \
         BEGIN SELECT RAISE(ABORT, 'injected'); END",
    )
    .execute(&pool)
    .await
    .unwrap();

    assert!(finalize(&ctx, &mut order, parties).await.unwrap());

    assert_finalized_without_history(&pool, &order).await;
}

#[tokio::test]
async fn an_already_finalized_order_records_nothing_and_keeps_its_stamp() {
    init_global_config();
    // Reaches the publish path, which writes the global republish queue.
    let _queue = ORDERBOOK_QUEUE_TEST_LOCK.lock().await;
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
    assert_eq!(
        orderbook_publish_attempts(order.id),
        None,
        "nothing published or armed"
    );
}

#[tokio::test]
async fn a_second_finalization_does_not_move_the_stamp() {
    init_global_config();
    // Reaches the publish path, which writes the global republish queue.
    let _queue = ORDERBOOK_QUEUE_TEST_LOCK.lock().await;
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
        let trades = load_history(
            &pool,
            node_keys(),
            &parties.buyer_master.to_string(),
            &hash('a'),
        )
        .await
        .unwrap()
        .successful_trades;
        assert!(trades <= 1, "at most one increment per order");
    }
}
