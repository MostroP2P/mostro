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

/// Node secret the history helpers key rows with in these tests.
fn node_keys() -> &'static Keys {
    static NODE: std::sync::OnceLock<Keys> = std::sync::OnceLock::new();
    NODE.get_or_init(Keys::generate)
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
    bump_history(&mut conn, node_keys(), user, h, cp, exp, generation, now)
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

/// Fingerprint of [`node_keys`], which [`seed_policy`] records the policy
/// under: a recompute with the same node is a threshold change, not a key
/// rotation.
fn seed_node() -> String {
    crate::app::payer::node_key_id(node_keys())
}

async fn seed_policy(pool: &SqlitePool, n: u32, d: u32) -> i64 {
    let mut conn = pool.acquire().await.unwrap();
    store_experience_policy(&mut conn, &seed_node(), n, d, NOW)
        .await
        .unwrap()
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
    let (seller, buyer) = (key(), key());
    let mut open = Trade::success(&seller, &buyer, NOW);
    open.status = Status::Active;
    let order_id = insert(&pool, open).await;

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
async fn a_declaration_belongs_to_the_buyer_who_made_it() {
    // A take that rolls back to pending (taker timeout) gives the order a new
    // buyer later; the previous buyer's declaration must not carry over.
    let pool = pool().await;
    let (seller, buyer) = (key(), key());
    let mut open = Trade::success(&seller, &buyer, NOW);
    open.status = Status::Active;
    let order_id = insert(&pool, open).await;
    upsert_declaration(&pool, order_id, &hash('a'), NOW)
        .await
        .unwrap();
    assert!(find_declaration(&pool, order_id).await.unwrap().is_some());

    sqlx::query("UPDATE orders SET buyer_pubkey = ?1 WHERE id = ?2")
        .bind(key())
        .bind(order_id)
        .execute(&pool)
        .await
        .unwrap();

    assert!(
        find_declaration(&pool, order_id).await.unwrap().is_none(),
        "not the current buyer's declaration"
    );
    let mut conn = pool.acquire().await.unwrap();
    assert!(
        take_declaration(&mut conn, order_id)
            .await
            .unwrap()
            .is_none(),
        "never recorded for the new buyer"
    );
    drop(conn);
    let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM order_payer_declarations")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(left, 0, "the stale row is consumed");
}

#[tokio::test]
async fn a_take_rolled_back_to_pending_voids_the_declaration() {
    // Even when the same trade key takes the order again, a new take starts
    // with no declaration.
    let pool = pool().await;
    let (seller, buyer) = (key(), key());
    let mut taken = Trade::success(&seller, &buyer, NOW);
    taken.status = Status::WaitingBuyerInvoice;
    let order_id = insert(&pool, taken).await;
    upsert_declaration(&pool, order_id, &hash('a'), NOW)
        .await
        .unwrap();

    assert!(
        crate::db::update_order_to_initial_state(&pool, order_id, 21_000, 0, 0)
            .await
            .unwrap()
    );

    sqlx::query("UPDATE orders SET buyer_pubkey = ?1 WHERE id = ?2")
        .bind(&buyer)
        .bind(order_id)
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        find_declaration(&pool, order_id).await.unwrap().is_none(),
        "the same buyer retaking the order does not inherit it"
    );
}

#[tokio::test]
async fn open_declaration_upsert_only_lands_while_the_window_is_open() {
    // The status check and the write are one statement, so a declaration
    // cannot land after a concurrent `fiat-sent` froze the order (§5.1).
    let pool = pool().await;
    let (seller, buyer) = (key(), key());
    let mut active = Trade::success(&seller, &buyer, NOW);
    active.status = Status::Active;
    let active = insert(&pool, active).await;
    let mut frozen = Trade::success(&seller, &buyer, NOW);
    frozen.status = Status::FiatSent;
    let frozen = insert(&pool, frozen).await;
    let buyer_key = PublicKey::from_hex(&buyer).unwrap();

    assert!(upsert_open_declaration(
        &pool,
        active,
        &buyer_key,
        Some(seller.as_str()),
        0,
        &hash('a'),
        1
    )
    .await
    .unwrap());
    assert!(upsert_open_declaration(
        &pool,
        active,
        &buyer_key,
        Some(seller.as_str()),
        0,
        &hash('b'),
        2
    )
    .await
    .unwrap());
    assert_eq!(
        find_declaration(&pool, active)
            .await
            .unwrap()
            .map(|d| d.payment_hash),
        Some(hash('b'))
    );

    assert!(!upsert_open_declaration(
        &pool,
        frozen,
        &buyer_key,
        Some(seller.as_str()),
        0,
        &hash('a'),
        1
    )
    .await
    .unwrap());
    assert!(find_declaration(&pool, frozen).await.unwrap().is_none());
    // Not the order's buyer (a request that raced a rollback and a new
    // take): refused, and the stored declaration is untouched.
    let other = Keys::generate().public_key();
    assert!(!upsert_open_declaration(
        &pool,
        active,
        &other,
        Some(seller.as_str()),
        0,
        &hash('c'),
        3
    )
    .await
    .unwrap());
    assert_eq!(
        find_declaration(&pool, active)
            .await
            .unwrap()
            .map(|d| d.payment_hash),
        Some(hash('b'))
    );
    // Same buyer, another seller (a maker-buyer order retaken meanwhile).
    let new_seller = key();
    assert!(!upsert_open_declaration(
        &pool,
        active,
        &buyer_key,
        Some(new_seller.as_str()),
        0,
        &hash('d'),
        4
    )
    .await
    .unwrap());
    // Same two keys, but another take (`taken_at` moved): a request read
    // before a rollback and retake must not attach to the new take.
    sqlx::query("UPDATE orders SET taken_at = 77 WHERE id = ?1")
        .bind(active)
        .execute(&pool)
        .await
        .unwrap();
    assert!(!upsert_open_declaration(
        &pool,
        active,
        &buyer_key,
        Some(seller.as_str()),
        0,
        &hash('e'),
        5
    )
    .await
    .unwrap());
    // Unknown order: nothing to attach the declaration to.
    assert!(!upsert_open_declaration(
        &pool,
        Uuid::new_v4(),
        &buyer_key,
        Some(seller.as_str()),
        0,
        &hash('a'),
        1
    )
    .await
    .unwrap());
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
    let h = load_history(&pool, node_keys(), &key(), &hash('d'))
        .await
        .unwrap();
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

    let got = load_history(&pool, node_keys(), &user, &h).await.unwrap();
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
        load_history(&pool, node_keys(), &user, &hash('f'))
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
    let got = load_history(&pool, node_keys(), &user, &h).await.unwrap();
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
async fn out_of_order_successes_keep_the_timestamp_extrema() {
    // A success stamped earlier can commit after a later one.
    let pool = pool().await;
    let generation = seed_policy(&pool, 5, 30).await;
    let (user, h) = (key(), hash('4'));
    bump(&pool, &user, &h, "cp", false, generation, 200).await;
    bump(&pool, &user, &h, "cp", false, generation, 100).await;

    let got = load_history(&pool, node_keys(), &user, &h).await.unwrap();
    assert_eq!(
        (got.first_success_at, got.last_success_at),
        (Some(100), Some(200))
    );
    let cp: (i64, i64) = sqlx::query_as(
        "SELECT first_success_at, last_success_at FROM payer_history_counterparties \
          WHERE user_pubkey = ?1",
    )
    .bind(&user)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(cp, (100, 200));
}

#[tokio::test]
async fn bump_history_rejects_a_malformed_hash() {
    let pool = pool().await;
    let mut conn = pool.acquire().await.unwrap();
    let err = bump_history(&mut conn, node_keys(), &key(), "NOT-HEX", "cp", false, 1, 1).await;
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
async fn seller_age_counts_from_success_not_from_order_creation() {
    // An offer that waited 39 days and succeeded yesterday is one day old
    // as evidence, not 40.
    let pool = pool().await;
    let (seller, buyer) = (key(), key());
    insert(
        &pool,
        Trade {
            success_at: Some(NOW - ONE_DAY),
            ..Trade::success(&seller, &key(), NOW - 40 * ONE_DAY)
        },
    )
    .await;
    let exp = experience(&pool, &seller, &buyer, None, None).await;
    assert_eq!(exp.first_qualifying_at, Some(NOW - ONE_DAY));

    // A legacy row with no success stamp falls back to its creation time.
    insert(
        &pool,
        Trade {
            success_at: None,
            ..Trade::success(&seller, &key(), NOW - 50 * ONE_DAY)
        },
    )
    .await;
    let exp = experience(&pool, &seller, &buyer, None, None).await;
    assert_eq!(
        (exp.qualifying_trades, exp.first_qualifying_at),
        (2, Some(NOW - 50 * ONE_DAY))
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
        current_policy_generation(&mut conn, node_keys(), (5, 30), NOW)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        current_policy_generation(&mut conn, node_keys(), (5, 30), NOW + 1)
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
            min_days: 30,
            node_key_id: Some(seed_node()),
        }),
        "the success path seeds the key its rows are written under"
    );

    assert_eq!(seed_policy(&pool, 3, 7).await, 2);
    assert_eq!(
        load_experience_policy(&pool).await.unwrap(),
        Some(ExperiencePolicy {
            generation: 2,
            min_trades: 3,
            min_days: 7,
            node_key_id: Some(seed_node()),
        })
    );
}

#[tokio::test]
async fn snapshots_under_unrecomputed_thresholds_are_never_counted() {
    // Thresholds changed but the recompute has not run yet: a snapshot taken
    // now must not join the stored generation, whose rows were evaluated
    // under the old thresholds.
    let pool = pool().await;
    let old = seed_policy(&pool, 5, 30).await;
    let (user, h) = (key(), hash('3'));
    bump(&pool, &user, &h, "cp-a", true, old, 100).await;

    let mut conn = pool.acquire().await.unwrap();
    let stale = current_policy_generation(&mut conn, node_keys(), (3, 7), NOW)
        .await
        .unwrap();
    drop(conn);
    assert_ne!(stale, old, "never the stored generation");
    bump(&pool, &user, &h, "cp-b", true, stale, 200).await;

    let got = load_history(&pool, node_keys(), &user, &h).await.unwrap();
    assert_eq!(got.distinct_counterparties, 2);
    assert_eq!(
        got.experienced_counterparties, 1,
        "only the snapshot evaluated under the stored thresholds"
    );
    assert_eq!(
        load_experience_policy(&pool)
            .await
            .unwrap()
            .unwrap()
            .generation,
        old,
        "the policy row is left to the recompute"
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
    let node = Keys::generate();
    let out = recompute_experienced(&pool, &node, 5, 30, NOW)
        .await
        .unwrap();
    assert_eq!(out, RecomputeOutcome::default());
    let policy = load_experience_policy(&pool).await.unwrap().unwrap();
    assert_eq!(policy.generation, 1);
    assert_eq!(
        policy.node_key_id,
        Some(crate::app::payer::node_key_id(&node)),
        "the recompute records the key it resolved ids with"
    );
}

#[tokio::test]
async fn recompute_upgrades_when_lowered_and_downgrades_when_raised() {
    let pool = pool().await;
    let node = node_keys().clone();
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
            unresolved: 0,
            discarded: 0,
        }
    );
    assert_eq!(
        load_history(&pool, node_keys(), &buyer, &hash('9'))
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
        load_history(&pool, node_keys(), &buyer, &hash('9'))
            .await
            .unwrap()
            .experienced_counterparties,
        0
    );
}

#[tokio::test]
async fn recompute_is_idempotent() {
    let pool = pool().await;
    let node = node_keys().clone();
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
    let node = node_keys().clone();
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
    let node = node_keys().clone();
    let old = seed_policy(&pool, 5, 30).await;
    let (buyer, h) = (key(), hash('9'));
    // A counterparty id no seller key in `orders` hashes to (an imported
    // database; a rotated node key discards the history instead).
    bump(&pool, &buyer, &h, &hash('7'), true, old, NOW).await;

    let out = recompute_experienced(&pool, &node, 1, 1, NOW)
        .await
        .unwrap();
    assert_eq!(
        out,
        RecomputeOutcome {
            changed: 0,
            unresolved: 1,
            discarded: 0,
        }
    );
    assert_eq!(
        stored_flag(&pool, &buyer).await,
        (1, -1),
        "value kept, marked unresolved"
    );

    let got = load_history(&pool, node_keys(), &buyer, &h).await.unwrap();
    assert_eq!(got.successful_trades, 1, "the trade itself still happened");
    assert_eq!(
        (got.distinct_counterparties, got.experienced_counterparties),
        (0, 0)
    );

    // The same seller trading again under the new key gets a new id; it must
    // count once, not next to its unresolvable old row.
    let current = load_experience_policy(&pool).await.unwrap().unwrap();
    bump(
        &pool,
        &buyer,
        &h,
        &hash('8'),
        false,
        current.generation,
        NOW + 1,
    )
    .await;
    let got = load_history(&pool, node_keys(), &buyer, &h).await.unwrap();
    assert_eq!(got.distinct_counterparties, 1);
}

#[tokio::test]
async fn recompute_discards_frozen_history_snapshots() {
    // A snapshot frozen under the old thresholds would contradict the policy
    // the info event now advertises; the next query re-takes it.
    let pool = pool().await;
    let (seller, buyer) = (key(), key());
    let mut open = Trade::success(&seller, &buyer, NOW);
    open.status = Status::FiatSent;
    let order_id = insert(&pool, open).await;
    upsert_declaration(&pool, order_id, &hash('a'), NOW)
        .await
        .unwrap();
    sqlx::query("UPDATE order_payer_declarations SET history_snapshot = '{}' WHERE order_id = ?1")
        .bind(order_id)
        .execute(&pool)
        .await
        .unwrap();

    recompute_experienced(&pool, &Keys::generate(), 3, 7, NOW)
        .await
        .unwrap();

    let snapshot: Option<String> = sqlx::query_scalar(
        "SELECT history_snapshot FROM order_payer_declarations WHERE order_id = ?1",
    )
    .bind(order_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(snapshot, None);
}

#[tokio::test]
async fn recompute_rolls_back_with_the_policy_row() {
    // The policy bump and the column rewrite share one transaction: when the
    // pass fails, neither is applied, so the next boot retries.
    let pool = pool().await;
    let node = node_keys().clone();
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
            min_days: 30,
            node_key_id: Some(seed_node()),
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

/// Every text value stored in the two history tables.
async fn stored_history_values(pool: &SqlitePool) -> Vec<String> {
    let mut values = Vec::new();
    for sql in [
        "SELECT * FROM payer_history",
        "SELECT * FROM payer_history_counterparties",
    ] {
        let rows = sqlx::query(sql).fetch_all(pool).await.unwrap();
        for row in rows {
            for i in 0..row.columns().len() {
                if let Ok(v) = row.try_get::<String, _>(i) {
                    values.push(v);
                }
            }
        }
    }
    values
}

#[tokio::test]
async fn history_never_stores_the_declared_hash() {
    // D-2: the hash is a low-entropy commitment anyone can recompute from a
    // candidate account; next to the buyer's identity key it would let a
    // leaked database be brute-forced back to "this npub pays from X".
    let pool = pool().await;
    let generation = seed_policy(&pool, 5, 30).await;
    let (user, h) = (key(), hash('c'));
    bump(&pool, &user, &h, "cp-1", false, generation, NOW).await;

    let values = stored_history_values(&pool).await;
    assert!(!values.is_empty(), "the trade was recorded");
    assert!(
        !values.contains(&h),
        "the declared payment hash must not be stored in the history tables"
    );
    // ... yet the same declaration finds it again under the same node secret,
    // and under no other.
    assert_eq!(
        load_history(&pool, node_keys(), &user, &h)
            .await
            .unwrap()
            .successful_trades,
        1
    );
    assert_eq!(
        load_history(&pool, &Keys::generate(), &user, &h)
            .await
            .unwrap(),
        HistoryCounters::default()
    );
}

#[tokio::test]
async fn recompute_after_a_node_key_change_discards_the_history() {
    // History rows are keyed by the node secret: under a new secret no
    // declaration can reach them again, so they are dropped rather than kept
    // as unreachable data.
    let pool = pool().await;
    let old_node = Keys::generate();
    recompute_experienced(&pool, &old_node, 5, 30, NOW)
        .await
        .unwrap();
    let generation = load_experience_policy(&pool)
        .await
        .unwrap()
        .unwrap()
        .generation;
    seed_snapshot(&pool, &old_node, &key(), 3, 40, NOW, generation).await;
    assert!(!stored_history_values(&pool).await.is_empty());

    let out = recompute_experienced(&pool, &Keys::generate(), 5, 30, NOW + 1)
        .await
        .unwrap();
    assert_eq!(out.discarded, 1);
    assert!(
        stored_history_values(&pool).await.is_empty(),
        "history keyed by the old node secret is discarded"
    );
}

/// Store a policy row that predates recording the node key.
async fn seed_policy_without_node_key(pool: &SqlitePool) -> i64 {
    seed_policy(pool, 5, 30).await;
    sqlx::query("UPDATE payer_history_policy SET node_key_id = NULL")
        .execute(pool)
        .await
        .unwrap();
    load_experience_policy(pool)
        .await
        .unwrap()
        .unwrap()
        .generation
}

#[tokio::test]
async fn recompute_under_the_same_node_key_discards_nothing() {
    let pool = pool().await;
    let generation = seed_policy(&pool, 5, 30).await;
    seed_snapshot(&pool, node_keys(), &key(), 3, 40, NOW, generation).await;
    let before = stored_history_values(&pool).await;

    let out = recompute_experienced(&pool, node_keys(), 3, 30, NOW + 1)
        .await
        .unwrap();
    assert_eq!(out.discarded, 0);
    assert_eq!(stored_history_values(&pool).await, before);
}

#[tokio::test]
async fn recompute_keeps_history_under_a_policy_with_no_recorded_key() {
    let pool = pool().await;
    let generation = seed_policy_without_node_key(&pool).await;
    seed_snapshot(&pool, node_keys(), &key(), 3, 40, NOW, generation).await;

    let out = recompute_experienced(&pool, node_keys(), 5, 30, NOW + 1)
        .await
        .unwrap();
    assert_eq!(out.discarded, 0, "an unknown key proves no rotation");
    assert!(!stored_history_values(&pool).await.is_empty());
    assert_eq!(
        load_experience_policy(&pool)
            .await
            .unwrap()
            .unwrap()
            .node_key_id,
        Some(seed_node()),
        "the recompute records the key from now on"
    );
}

#[tokio::test]
async fn discard_counts_history_rows_not_counterparty_rows() {
    let pool = pool().await;
    let generation = seed_policy(&pool, 5, 30).await;
    let (user, h) = (key(), hash('4'));
    for cp in ["cp-1", "cp-2", "cp-3"] {
        bump(&pool, &user, &h, cp, false, generation, NOW).await;
    }
    bump(&pool, &user, &hash('5'), "cp-1", false, generation, NOW).await;

    let out = recompute_experienced(&pool, &Keys::generate(), 5, 30, NOW + 1)
        .await
        .unwrap();
    assert_eq!(out.discarded, 2, "two (buyer, account) histories");
    assert!(stored_history_values(&pool).await.is_empty());
}

#[tokio::test]
async fn a_failed_recompute_restores_discarded_history() {
    // The discard shares the recompute transaction: if the pass fails after
    // dropping the rows, they come back with the old policy row.
    let pool = pool().await;
    let generation = seed_policy(&pool, 5, 30).await;
    seed_snapshot(&pool, node_keys(), &key(), 3, 40, NOW, generation).await;
    let before = stored_history_values(&pool).await;
    sqlx::query("DROP TABLE orders")
        .execute(&pool)
        .await
        .unwrap();

    assert!(
        recompute_experienced(&pool, &Keys::generate(), 5, 30, NOW + 1)
            .await
            .is_err()
    );
    assert_eq!(stored_history_values(&pool).await, before);
    assert_eq!(
        load_experience_policy(&pool)
            .await
            .unwrap()
            .unwrap()
            .node_key_id,
        Some(seed_node())
    );
}
