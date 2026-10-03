//! Tests for `payer::db` (spec §14). Loaded from `db.rs` via `#[path]` so
//! the storage module stays readable.

use super::*;
use mostro_core::db::Crud;
use mostro_core::order::{Order, Status};
use sqlx::SqlitePool;

const ONE_DAY: i64 = 86_400;
const NOW: i64 = 2_000 * ONE_DAY;

async fn pool() -> SqlitePool {
    let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
    sqlx::migrate!().run(&pool).await.unwrap();
    pool
}

fn hash(c: char) -> String {
    std::iter::repeat_n(c, 64).collect()
}

fn key() -> String {
    Keys::generate().public_key().to_string()
}

/// A finished order between `seller` and `buyer` (master keys), created at
/// `created_at` and stamped `success_at` when given.
struct Trade<'a> {
    seller: &'a str,
    buyer: &'a str,
    status: Status,
    created_at: i64,
    success_at: Option<i64>,
    disputed: bool,
}

impl<'a> Trade<'a> {
    fn success(seller: &'a str, buyer: &'a str, created_at: i64) -> Self {
        Self {
            seller,
            buyer,
            status: Status::Success,
            created_at,
            success_at: Some(created_at),
            disputed: false,
        }
    }
}

async fn insert(pool: &SqlitePool, t: Trade<'_>) -> Uuid {
    let order = Order {
        id: Uuid::new_v4(),
        status: t.status.to_string(),
        kind: mostro_core::order::Kind::Sell.to_string(),
        fiat_code: "USD".to_string(),
        creator_pubkey: t.seller.to_string(),
        seller_pubkey: Some(t.seller.to_string()),
        master_seller_pubkey: Some(t.seller.to_string()),
        buyer_pubkey: Some(t.buyer.to_string()),
        master_buyer_pubkey: Some(t.buyer.to_string()),
        amount: 21_000,
        fiat_amount: 40,
        created_at: t.created_at,
        buyer_dispute: t.disputed,
        ..Default::default()
    }
    .create(pool)
    .await
    .unwrap();
    sqlx::query("UPDATE orders SET success_at = ?1 WHERE id = ?2")
        .bind(t.success_at)
        .bind(order.id)
        .execute(pool)
        .await
        .unwrap();
    order.id
}

async fn bump(
    pool: &SqlitePool,
    user: &str,
    h: &str,
    cp: &str,
    exp: bool,
    generation: i64,
    now: i64,
) {
    let mut conn = pool.acquire().await.unwrap();
    bump_history(&mut conn, user, h, cp, exp, generation, now)
        .await
        .unwrap();
}

async fn experience(
    pool: &SqlitePool,
    seller: &str,
    buyer: &str,
    current: Option<Uuid>,
    as_of: Option<i64>,
) -> SellerExperience {
    let mut conn = pool.acquire().await.unwrap();
    seller_experience(&mut conn, seller, buyer, current, as_of)
        .await
        .unwrap()
}

async fn seed_policy(pool: &SqlitePool, n: u32, d: u32) -> i64 {
    let mut conn = pool.acquire().await.unwrap();
    store_experience_policy(&mut conn, n, d, NOW).await.unwrap()
}

async fn stored_flag(pool: &SqlitePool, user: &str) -> (i64, i64) {
    sqlx::query_as::<_, (i64, i64)>(
        "SELECT experienced, policy_gen FROM payer_history_counterparties WHERE user_pubkey = ?1",
    )
    .bind(user)
    .fetch_one(pool)
    .await
    .unwrap()
}

// ---------------------------------------------------------------- declarations

#[tokio::test]
async fn declaration_upsert_overwrites_and_take_consumes_once() {
    let pool = pool().await;
    let order_id = Uuid::new_v4();

    upsert_declaration(&pool, order_id, &hash('a'), 10)
        .await
        .unwrap();
    upsert_declaration(&pool, order_id, &hash('b'), 20)
        .await
        .unwrap();

    let found = find_declaration(&pool, order_id).await.unwrap().unwrap();
    assert_eq!(found.payment_hash, hash('b'), "last write wins");
    assert_eq!(found.declared_at, 20);

    let mut conn = pool.acquire().await.unwrap();
    let taken = take_declaration(&mut conn, order_id).await.unwrap();
    assert_eq!(taken.map(|d| d.payment_hash), Some(hash('b')));
    assert!(take_declaration(&mut conn, order_id)
        .await
        .unwrap()
        .is_none());
    assert!(find_declaration(&pool, order_id).await.unwrap().is_none());
}

#[tokio::test]
async fn prune_removes_terminal_declarations_and_keeps_active_ones() {
    let pool = pool().await;
    let (seller, buyer) = (key(), key());
    let mut canceled = Trade::success(&seller, &buyer, NOW);
    canceled.status = Status::Canceled;
    let canceled = insert(&pool, canceled).await;
    let mut active = Trade::success(&seller, &buyer, NOW);
    active.status = Status::Active;
    let active = insert(&pool, active).await;
    // A Success order whose declaration was never consumed (feature turned
    // off before the trade finished, D-10) is terminal too.
    let succeeded = insert(&pool, Trade::success(&seller, &buyer, NOW)).await;
    for id in [canceled, active, succeeded] {
        upsert_declaration(&pool, id, &hash('c'), NOW)
            .await
            .unwrap();
    }

    assert_eq!(
        prune_declarations_for_terminal_orders(&pool).await.unwrap(),
        2
    );

    assert!(find_declaration(&pool, canceled).await.unwrap().is_none());
    assert!(find_declaration(&pool, succeeded).await.unwrap().is_none());
    assert!(find_declaration(&pool, active).await.unwrap().is_some());
}

#[tokio::test]
async fn prune_query_never_scans_orders() {
    // §10.6: the job runs on every node, so the DELETE must be driven by the
    // declarations table. `orders.status` has no index, and the uncorrelated
    // `IN (SELECT …)` form would scan `orders` on every pass.
    let pool = pool().await;
    let rows = sqlx::query(AssertSqlSafe(format!("EXPLAIN QUERY PLAN {}", prune_sql())))
        .fetch_all(&pool)
        .await
        .unwrap();
    let details: Vec<String> = rows.iter().map(|r| r.get::<String, _>("detail")).collect();
    assert!(
        details
            .iter()
            .any(|d| d.starts_with("SCAN order_payer_declarations")),
        "prune plan is not driven by the declarations table: {details:?}"
    );
    assert!(
        // `orders` is aliased `o` in the subquery; a scan would read
        // "SCAN o" (or "SCAN orders" without the alias).
        details
            .iter()
            .all(|d| d != "SCAN o" && !d.starts_with("SCAN o ") && !d.starts_with("SCAN orders")),
        "prune plan scans orders: {details:?}"
    );
}

// --------------------------------------------------------------------- history

#[tokio::test]
async fn load_history_is_zero_without_rows() {
    let pool = pool().await;
    let h = load_history(&pool, &key(), &hash('d')).await.unwrap();
    assert_eq!(h, HistoryCounters::default());
    assert_eq!((h.first_success_at, h.last_success_at), (None, None));
}

#[tokio::test]
async fn bump_history_counts_trades_and_distinct_counterparties() {
    let pool = pool().await;
    let generation = seed_policy(&pool, 5, 30).await;
    let (user, h) = (key(), hash('e'));

    bump(&pool, &user, &h, "cp-1", false, generation, 100).await;
    bump(&pool, &user, &h, "cp-1", false, generation, 200).await;
    bump(&pool, &user, &h, "cp-2", true, generation, 300).await;

    let got = load_history(&pool, &user, &h).await.unwrap();
    assert_eq!(got.successful_trades, 3);
    assert_eq!(
        got.distinct_counterparties, 2,
        "same seller twice counts once"
    );
    assert_eq!(got.experienced_counterparties, 1);
    assert_eq!(got.first_success_at, Some(100));
    assert_eq!(got.last_success_at, Some(300));
    // Another hash of the same user is a separate history.
    assert_eq!(
        load_history(&pool, &user, &hash('f'))
            .await
            .unwrap()
            .successful_trades,
        0
    );
}

#[tokio::test]
async fn experienced_flag_is_monotone_within_a_generation() {
    let pool = pool().await;
    let generation = seed_policy(&pool, 5, 30).await;
    let (user, h) = (key(), hash('1'));

    bump(&pool, &user, &h, "cp", false, generation, 100).await;
    assert_eq!(stored_flag(&pool, &user).await, (0, generation));
    bump(&pool, &user, &h, "cp", true, generation, 200).await;
    assert_eq!(
        stored_flag(&pool, &user).await,
        (1, generation),
        "0 → 1 upgrade"
    );
    bump(&pool, &user, &h, "cp", false, generation, 300).await;
    assert_eq!(
        stored_flag(&pool, &user).await,
        (1, generation),
        "never back to 0"
    );
}

#[tokio::test]
async fn stale_generation_rows_are_overwritten_and_not_counted() {
    let pool = pool().await;
    let old = seed_policy(&pool, 5, 30).await;
    let (user, h) = (key(), hash('2'));
    bump(&pool, &user, &h, "cp", true, old, 100).await;

    let current = seed_policy(&pool, 3, 7).await;
    assert_eq!(current, old + 1);
    let got = load_history(&pool, &user, &h).await.unwrap();
    assert_eq!(
        got.distinct_counterparties, 1,
        "stale row still counts as distinct"
    );
    assert_eq!(got.experienced_counterparties, 0, "stale 1 never counted");

    // A later success re-evaluates the row under the live policy: a stale 1
    // must not survive through MAX().
    bump(&pool, &user, &h, "cp", false, current, 200).await;
    assert_eq!(stored_flag(&pool, &user).await, (0, current));
}

#[tokio::test]
async fn bump_history_rejects_a_malformed_hash() {
    let pool = pool().await;
    let mut conn = pool.acquire().await.unwrap();
    let err = bump_history(&mut conn, &key(), "NOT-HEX", "cp", false, 1, 1).await;
    assert!(matches!(
        err,
        Err(MostroError::MostroCantDo(
            mostro_core::error::CantDoReason::InvalidPaymentHash
        ))
    ));
}

// ---------------------------------------------------------- seller experience

#[tokio::test]
async fn seller_experience_counts_only_undisputed_successes_with_other_buyers() {
    let pool = pool().await;
    let (seller, buyer, other) = (key(), key(), key());

    insert(&pool, Trade::success(&seller, &other, NOW - 40 * ONE_DAY)).await;
    insert(&pool, Trade::success(&seller, &key(), NOW - 10 * ONE_DAY)).await;
    // Same buyer: never qualifies (D-7 anti-Sybil).
    insert(&pool, Trade::success(&seller, &buyer, NOW - 90 * ONE_DAY)).await;
    // Disputed: counts toward neither N nor D (D-6).
    let mut disputed = Trade::success(&seller, &other, NOW - 80 * ONE_DAY);
    disputed.disputed = true;
    insert(&pool, disputed).await;
    // Not a success.
    let mut canceled = Trade::success(&seller, &other, NOW - 70 * ONE_DAY);
    canceled.status = Status::Canceled;
    insert(&pool, canceled).await;
    // Another seller entirely.
    insert(&pool, Trade::success(&key(), &other, NOW - 60 * ONE_DAY)).await;

    let exp = experience(&pool, &seller, &buyer, None, None).await;
    assert_eq!(exp.qualifying_trades, 2);
    assert_eq!(exp.first_qualifying_at, Some(NOW - 40 * ONE_DAY));
}

#[tokio::test]
async fn seller_experience_excludes_the_trade_being_recorded() {
    let pool = pool().await;
    let (seller, buyer) = (key(), key());
    let current = insert(&pool, Trade::success(&seller, &key(), NOW)).await;

    let exp = experience(&pool, &seller, &buyer, Some(current), None).await;
    assert_eq!(exp.qualifying_trades, 0);
    assert_eq!(exp.first_qualifying_at, None);
}

#[tokio::test]
async fn seller_experience_as_of_ignores_later_successes_but_counts_null_stamps() {
    let pool = pool().await;
    let (seller, buyer) = (key(), key());
    let snapshot = NOW - 5 * ONE_DAY;

    // Succeeded before the snapshot.
    insert(&pool, Trade::success(&seller, &key(), NOW - 20 * ONE_DAY)).await;
    // Pre-migration success (NULL stamp): predates every snapshot.
    let (b1, b2) = (key(), key());
    let mut legacy = Trade::success(&seller, &b1, NOW - 30 * ONE_DAY);
    legacy.success_at = None;
    insert(&pool, legacy).await;
    // Created before the snapshot but only succeeded after it.
    let mut late = Trade::success(&seller, &b2, NOW - 10 * ONE_DAY);
    late.success_at = Some(NOW);
    insert(&pool, late).await;

    assert_eq!(
        experience(&pool, &seller, &buyer, None, Some(snapshot))
            .await
            .qualifying_trades,
        2
    );
    assert_eq!(
        experience(&pool, &seller, &buyer, None, None)
            .await
            .qualifying_trades,
        3
    );
}

#[tokio::test]
async fn seller_experience_as_of_excludes_a_success_at_the_same_instant() {
    let pool = pool().await;
    let (seller, buyer, other) = (key(), key(), key());
    let snapshot = NOW - 5 * ONE_DAY;
    let mut same_instant = Trade::success(&seller, &other, NOW - 20 * ONE_DAY);
    same_instant.success_at = Some(snapshot);
    insert(&pool, same_instant).await;

    let exp = experience(&pool, &seller, &buyer, None, Some(snapshot)).await;
    assert_eq!(
        exp.qualifying_trades, 0,
        "strictly before the snapshot only"
    );
}

#[tokio::test]
async fn seller_experience_ignores_trades_with_an_unknown_buyer() {
    let pool = pool().await;
    let (seller, buyer, other) = (key(), key(), key());
    let id = insert(&pool, Trade::success(&seller, &other, NOW - 40 * ONE_DAY)).await;
    sqlx::query("UPDATE orders SET master_buyer_pubkey = NULL WHERE id = ?1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();

    let exp = experience(&pool, &seller, &buyer, None, None).await;
    assert_eq!(
        exp.qualifying_trades, 0,
        "cannot be shown to be another buyer"
    );
}

// ---------------------------------------------------------------------- policy

#[tokio::test]
async fn policy_generation_seeds_once_and_bumps_on_store() {
    let pool = pool().await;
    assert!(load_experience_policy(&pool).await.unwrap().is_none());

    let mut conn = pool.acquire().await.unwrap();
    assert_eq!(
        current_policy_generation(&mut conn, (5, 30), NOW)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        current_policy_generation(&mut conn, (9, 9), NOW)
            .await
            .unwrap(),
        1,
        "seeded once"
    );
    drop(conn);
    assert_eq!(
        load_experience_policy(&pool).await.unwrap(),
        Some(ExperiencePolicy {
            generation: 1,
            min_trades: 5,
            min_days: 30
        })
    );

    assert_eq!(seed_policy(&pool, 3, 7).await, 2);
    assert_eq!(
        load_experience_policy(&pool).await.unwrap(),
        Some(ExperiencePolicy {
            generation: 2,
            min_trades: 3,
            min_days: 7
        })
    );
}

// ------------------------------------------------------------------- recompute

/// A buyer whose history has one counterparty `seller`, recorded at `at`,
/// where `seller` had `prior` successes with other buyers, the first one
/// `age_days` before `at`, stored with `experienced = 0`.
async fn seed_snapshot(
    pool: &SqlitePool,
    node: &Keys,
    seller: &str,
    prior: usize,
    age_days: i64,
    at: i64,
    generation: i64,
) -> String {
    let buyer = key();
    for i in 0..prior {
        let created = at - age_days * ONE_DAY + i as i64;
        insert(pool, Trade::success(seller, &key(), created)).await;
    }
    // The recorded trade itself (with this buyer).
    insert(pool, Trade::success(seller, &buyer, at)).await;
    bump(
        pool,
        &buyer,
        &hash('9'),
        &counterparty_id(node, seller),
        false,
        generation,
        at,
    )
    .await;
    buyer
}

#[tokio::test]
async fn recompute_on_empty_database_seeds_the_policy() {
    let pool = pool().await;
    let out = recompute_experienced(&pool, &Keys::generate(), 5, 30, NOW)
        .await
        .unwrap();
    assert_eq!(out, RecomputeOutcome::default());
    assert_eq!(
        load_experience_policy(&pool)
            .await
            .unwrap()
            .map(|p| p.generation),
        Some(1)
    );
}

#[tokio::test]
async fn recompute_upgrades_when_lowered_and_downgrades_when_raised() {
    let pool = pool().await;
    let node = Keys::generate();
    let generation = seed_policy(&pool, 5, 30).await;
    // 3 prior trades, 40 days old: fails N=5, passes N=3.
    let buyer = seed_snapshot(&pool, &node, &key(), 3, 40, NOW, generation).await;

    let lowered = recompute_experienced(&pool, &node, 3, 30, NOW)
        .await
        .unwrap();
    assert_eq!(
        lowered,
        RecomputeOutcome {
            changed: 1,
            unresolved: 0
        }
    );
    assert_eq!(
        load_history(&pool, &buyer, &hash('9'))
            .await
            .unwrap()
            .experienced_counterparties,
        1
    );

    let raised = recompute_experienced(&pool, &node, 4, 30, NOW)
        .await
        .unwrap();
    assert_eq!(
        raised.changed, 1,
        "1 → 0 is only reachable through a recompute"
    );
    assert_eq!(
        load_history(&pool, &buyer, &hash('9'))
            .await
            .unwrap()
            .experienced_counterparties,
        0
    );
}

#[tokio::test]
async fn recompute_is_idempotent() {
    let pool = pool().await;
    let node = Keys::generate();
    let generation = seed_policy(&pool, 5, 30).await;
    seed_snapshot(&pool, &node, &key(), 3, 40, NOW, generation).await;

    assert_eq!(
        recompute_experienced(&pool, &node, 3, 30, NOW)
            .await
            .unwrap()
            .changed,
        1
    );
    assert_eq!(
        recompute_experienced(&pool, &node, 3, 30, NOW)
            .await
            .unwrap()
            .changed,
        0
    );
}

#[tokio::test]
async fn recompute_evaluates_each_snapshot_at_its_own_instant() {
    let pool = pool().await;
    let node = Keys::generate();
    let generation = seed_policy(&pool, 5, 30).await;
    let seller = key();
    let snapshot = NOW - 100 * ONE_DAY;
    // At the snapshot the seller had a single prior trade...
    let buyer = seed_snapshot(&pool, &node, &seller, 1, 40, snapshot, generation).await;
    // ...and qualified only afterwards.
    for i in 0..5 {
        insert(
            &pool,
            Trade::success(&seller, &key(), snapshot + ONE_DAY + i),
        )
        .await;
    }

    let out = recompute_experienced(&pool, &node, 2, 30, NOW)
        .await
        .unwrap();
    assert_eq!(out.changed, 0);
    assert_eq!(stored_flag(&pool, &buyer).await.0, 0);
}

#[tokio::test]
async fn recompute_leaves_unresolvable_rows_stale_and_uncounted() {
    let pool = pool().await;
    let node = Keys::generate();
    let old = seed_policy(&pool, 5, 30).await;
    let (buyer, h) = (key(), hash('9'));
    // A counterparty id no seller key in `orders` hashes to (rotated key).
    bump(&pool, &buyer, &h, &hash('7'), true, old, NOW).await;

    let out = recompute_experienced(&pool, &node, 1, 1, NOW)
        .await
        .unwrap();
    assert_eq!(
        out,
        RecomputeOutcome {
            changed: 0,
            unresolved: 1
        }
    );
    assert_eq!(
        stored_flag(&pool, &buyer).await,
        (1, old),
        "value and generation kept"
    );

    let got = load_history(&pool, &buyer, &h).await.unwrap();
    assert_eq!(got.distinct_counterparties, 1);
    assert_eq!(got.experienced_counterparties, 0);
}

#[tokio::test]
async fn recompute_rolls_back_with_the_policy_row() {
    // The policy bump and the column rewrite share one transaction: when the
    // pass fails, neither is applied, so the next boot retries.
    let pool = pool().await;
    let node = Keys::generate();
    let generation = seed_policy(&pool, 5, 30).await;
    seed_snapshot(&pool, &node, &key(), 3, 40, NOW, generation).await;
    sqlx::query("DROP TABLE orders")
        .execute(&pool)
        .await
        .unwrap();

    assert!(recompute_experienced(&pool, &node, 3, 30, NOW)
        .await
        .is_err());
    assert_eq!(
        load_experience_policy(&pool).await.unwrap(),
        Some(ExperiencePolicy {
            generation,
            min_trades: 5,
            min_days: 30
        })
    );
}

#[tokio::test]
async fn success_at_column_exists_on_orders() {
    let pool = pool().await;
    let id = insert(&pool, Trade::success(&key(), &key(), NOW)).await;
    let stamped: Option<i64> = sqlx::query_scalar("SELECT success_at FROM orders WHERE id = ?1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(stamped, Some(NOW));
}
