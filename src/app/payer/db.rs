//! Storage for payer declarations and payment-account history
//! (`docs/PAYER_HISTORY_ANTI_TRIANGULATION.md` §9, §10.1).
//!
//! Functions that must run inside the Success CAS transaction take a
//! `&mut SqliteConnection` (pass `&mut *tx`); the rest take the pool.
//! Nothing here hits LND or Nostr, and nothing here checks the feature flag:
//! callers gate (D-10).

use std::collections::HashMap;

use mostro_core::error::{MostroError, MostroError::MostroInternalErr, ServiceError};
use nostr_sdk::prelude::Keys;
use sqlx::{AssertSqlSafe, Pool, Row, Sqlite, SqliteConnection};
use uuid::Uuid;

use super::{counterparty_id, is_experienced, validate_payment_hash};
use crate::db::TERMINAL_ORDER_STATUSES;

fn db_err(e: sqlx::Error) -> MostroError {
    MostroInternalErr(ServiceError::DbAccessError(e.to_string()))
}

/// One row of `order_payer_declarations`.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct PayerDeclarationRow {
    pub order_id: Uuid,
    pub payment_hash: String,
    pub declared_at: i64,
}

/// Insert or overwrite the declaration for `order_id` (last write wins).
pub async fn upsert_declaration(
    pool: &Pool<Sqlite>,
    order_id: Uuid,
    payment_hash: &str,
    now: i64,
) -> Result<(), MostroError> {
    sqlx::query(
        "INSERT INTO order_payer_declarations (order_id, payment_hash, declared_at) \
         VALUES (?1, ?2, ?3) \
         ON CONFLICT(order_id) DO UPDATE SET payment_hash = excluded.payment_hash, \
                                             declared_at = excluded.declared_at",
    )
    .bind(order_id)
    .bind(payment_hash)
    .bind(now)
    .execute(pool)
    .await
    .map_err(db_err)?;
    Ok(())
}

/// The current declaration for `order_id`, if any.
pub async fn find_declaration(
    pool: &Pool<Sqlite>,
    order_id: Uuid,
) -> Result<Option<PayerDeclarationRow>, MostroError> {
    sqlx::query_as::<_, PayerDeclarationRow>(
        "SELECT order_id, payment_hash, declared_at FROM order_payer_declarations \
         WHERE order_id = ?1",
    )
    .bind(order_id)
    .fetch_optional(pool)
    .await
    .map_err(db_err)
}

/// Delete and return the declaration for `order_id`. The row can be taken
/// exactly once, which makes it the idempotency token of the success hook.
pub async fn take_declaration(
    conn: &mut SqliteConnection,
    order_id: Uuid,
) -> Result<Option<PayerDeclarationRow>, MostroError> {
    sqlx::query_as::<_, PayerDeclarationRow>(
        "DELETE FROM order_payer_declarations WHERE order_id = ?1 \
         RETURNING order_id, payment_hash, declared_at",
    )
    .bind(order_id)
    .fetch_optional(conn)
    .await
    .map_err(db_err)
}

/// Delete the declarations of every order in a terminal status. Correlated
/// on purpose: the plan scans `order_payer_declarations` and looks each order
/// up by primary key, so it never scans `orders` (§10.6). Returns the number
/// of rows removed.
pub async fn prune_declarations_for_terminal_orders(
    pool: &Pool<Sqlite>,
) -> Result<u64, MostroError> {
    let done = sqlx::query(AssertSqlSafe(prune_sql()))
        .execute(pool)
        .await
        .map_err(db_err)?;
    Ok(done.rows_affected())
}

fn prune_sql() -> String {
    format!(
        "DELETE FROM order_payer_declarations \
         WHERE EXISTS (SELECT 1 FROM orders o \
                        WHERE o.id = order_payer_declarations.order_id \
                          AND o.status IN ({TERMINAL_ORDER_STATUSES}))"
    )
}

/// Aggregate counters for one `(buyer, payment_hash)` pair.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HistoryCounters {
    pub successful_trades: u32,
    pub distinct_counterparties: u32,
    /// Counterparties stored with `experienced = 1` under the CURRENT policy
    /// generation only (§9).
    pub experienced_counterparties: u32,
    pub first_success_at: Option<i64>,
    pub last_success_at: Option<i64>,
}

fn to_u32(n: i64) -> u32 {
    u32::try_from(n.max(0)).unwrap_or(u32::MAX)
}

/// Counters for `(user_pubkey, payment_hash)`; all zero when there is no
/// history. A row whose `policy_gen` is stale counts toward
/// `distinct_counterparties` but never toward `experienced_counterparties`,
/// so a count is always evaluated under the advertised thresholds (§10.7).
pub async fn load_history(
    pool: &Pool<Sqlite>,
    user_pubkey: &str,
    payment_hash: &str,
) -> Result<HistoryCounters, MostroError> {
    let head = sqlx::query(
        "SELECT successful_trades, first_success_at, last_success_at FROM payer_history \
         WHERE user_pubkey = ?1 AND payment_hash = ?2",
    )
    .bind(user_pubkey)
    .bind(payment_hash)
    .fetch_optional(pool)
    .await
    .map_err(db_err)?;
    let Some(head) = head else {
        return Ok(HistoryCounters::default());
    };
    let counts = sqlx::query(
        "SELECT COUNT(*) AS distinct_cp, \
                COALESCE(SUM(CASE WHEN c.experienced = 1 \
                                   AND c.policy_gen = (SELECT generation FROM payer_history_policy WHERE id = 1) \
                                  THEN 1 ELSE 0 END), 0) AS experienced_cp \
           FROM payer_history_counterparties c \
          WHERE c.user_pubkey = ?1 AND c.payment_hash = ?2",
    )
    .bind(user_pubkey)
    .bind(payment_hash)
    .fetch_one(pool)
    .await
    .map_err(db_err)?;
    Ok(HistoryCounters {
        successful_trades: to_u32(head.get::<i64, _>("successful_trades")),
        distinct_counterparties: to_u32(counts.get::<i64, _>("distinct_cp")),
        experienced_counterparties: to_u32(counts.get::<i64, _>("experienced_cp")),
        first_success_at: Some(head.get::<i64, _>("first_success_at")),
        last_success_at: Some(head.get::<i64, _>("last_success_at")),
    })
}

/// Record one successful trade for `(user_pubkey, payment_hash)` with the
/// counterparty `counterparty_id`. Must run inside the Success CAS
/// transaction. Under a fixed policy generation the counterparty's
/// `experienced` flag only ever goes 0 → 1; a row left stale by §10.7 is
/// overwritten outright and rejoins the current generation.
pub async fn bump_history(
    conn: &mut SqliteConnection,
    user_pubkey: &str,
    payment_hash: &str,
    counterparty_id: &str,
    experienced: bool,
    policy_gen: i64,
    now: i64,
) -> Result<(), MostroError> {
    validate_payment_hash(payment_hash)?;
    sqlx::query(
        "INSERT INTO payer_history \
           (user_pubkey, payment_hash, first_success_at, last_success_at, successful_trades) \
         VALUES (?1, ?2, ?3, ?3, 1) \
         ON CONFLICT(user_pubkey, payment_hash) \
         DO UPDATE SET last_success_at = excluded.last_success_at, \
                       successful_trades = successful_trades + 1",
    )
    .bind(user_pubkey)
    .bind(payment_hash)
    .bind(now)
    .execute(&mut *conn)
    .await
    .map_err(db_err)?;
    sqlx::query(
        "INSERT INTO payer_history_counterparties \
           (user_pubkey, payment_hash, counterparty_id, first_success_at, last_success_at, \
            experienced, policy_gen) \
         VALUES (?1, ?2, ?3, ?4, ?4, ?5, ?6) \
         ON CONFLICT(user_pubkey, payment_hash, counterparty_id) \
         DO UPDATE SET last_success_at = excluded.last_success_at, \
                       experienced = CASE WHEN policy_gen = excluded.policy_gen \
                                          THEN MAX(experienced, excluded.experienced) \
                                          ELSE excluded.experienced END, \
                       policy_gen = excluded.policy_gen",
    )
    .bind(user_pubkey)
    .bind(payment_hash)
    .bind(counterparty_id)
    .bind(now)
    .bind(i64::from(experienced))
    .bind(policy_gen)
    .execute(&mut *conn)
    .await
    .map_err(db_err)?;
    Ok(())
}

/// A seller's qualifying record for the D-7 predicate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SellerExperience {
    pub qualifying_trades: u32,
    pub first_qualifying_at: Option<i64>,
}

/// D-7 qualification input, read from `orders` (not from the history
/// tables), so a seller's whole node-side record counts, including trades
/// that predate this feature. Only undisputed `success` trades count, and
/// only those with a buyer OTHER than `buyer_pubkey` (anti-Sybil). An order
/// whose `master_buyer_pubkey` is NULL is excluded too, since it cannot be
/// shown to be a different buyer.
///
/// * `current_order`: the trade being recorded, which never counts toward
///   its own qualification. `None` on the recompute path (§10.7).
/// * `as_of`: `None` on the live success path; `Some(instant)` when
///   re-evaluating a stored snapshot, which must see only trades that had
///   already reached Success at that instant (`success_at < as_of`; a NULL
///   `success_at` predates every snapshot and counts).
pub async fn seller_experience(
    conn: &mut SqliteConnection,
    seller_master_pubkey: &str,
    buyer_pubkey: &str,
    current_order: Option<Uuid>,
    as_of: Option<i64>,
) -> Result<SellerExperience, MostroError> {
    let row = sqlx::query(
        "SELECT COUNT(*) AS n, MIN(created_at) AS first_at \
           FROM orders \
          WHERE master_seller_pubkey = ?1 \
            AND status = 'success' \
            AND buyer_dispute = 0 AND seller_dispute = 0 \
            AND (?2 IS NULL OR id <> ?2) \
            AND master_buyer_pubkey <> ?3 \
            AND (?4 IS NULL OR success_at IS NULL OR success_at < ?4)",
    )
    .bind(seller_master_pubkey)
    .bind(current_order)
    .bind(buyer_pubkey)
    .bind(as_of)
    .fetch_one(conn)
    .await
    .map_err(db_err)?;
    Ok(SellerExperience {
        qualifying_trades: to_u32(row.get::<i64, _>("n")),
        first_qualifying_at: row.get::<Option<i64>, _>("first_at"),
    })
}

/// Threshold policy the stored `experienced` snapshots were evaluated under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExperiencePolicy {
    pub generation: i64,
    pub min_trades: u32,
    pub min_days: u32,
}

/// The stored policy, or `None` when the feature never ran on this database.
pub async fn load_experience_policy(
    pool: &Pool<Sqlite>,
) -> Result<Option<ExperiencePolicy>, MostroError> {
    let row = sqlx::query(
        "SELECT generation, experienced_min_trades, experienced_min_days \
           FROM payer_history_policy WHERE id = 1",
    )
    .fetch_optional(pool)
    .await
    .map_err(db_err)?;
    Ok(row.map(|r| ExperiencePolicy {
        generation: r.get("generation"),
        min_trades: to_u32(r.get("experienced_min_trades")),
        min_days: to_u32(r.get("experienced_min_days")),
    }))
}

/// Record `(min_trades, min_days)` as the current policy, bumping the
/// generation (1 on first use). Returns the new generation.
pub async fn store_experience_policy(
    conn: &mut SqliteConnection,
    min_trades: u32,
    min_days: u32,
    now: i64,
) -> Result<i64, MostroError> {
    sqlx::query_scalar::<_, i64>(
        "INSERT INTO payer_history_policy \
           (id, generation, experienced_min_trades, experienced_min_days, evaluated_at) \
         VALUES (1, 1, ?1, ?2, ?3) \
         ON CONFLICT(id) DO UPDATE SET generation = generation + 1, \
                                       experienced_min_trades = excluded.experienced_min_trades, \
                                       experienced_min_days = excluded.experienced_min_days, \
                                       evaluated_at = excluded.evaluated_at \
         RETURNING generation",
    )
    .bind(i64::from(min_trades))
    .bind(i64::from(min_days))
    .bind(now)
    .fetch_one(conn)
    .await
    .map_err(db_err)
}

/// Generation to stamp into a snapshot taken right now (§10.5). Seeds the
/// policy row with `thresholds` on first use, so the success path never
/// races the boot-time recompute.
pub async fn current_policy_generation(
    conn: &mut SqliteConnection,
    thresholds: (u32, u32),
    now: i64,
) -> Result<i64, MostroError> {
    let existing =
        sqlx::query_scalar::<_, i64>("SELECT generation FROM payer_history_policy WHERE id = 1")
            .fetch_optional(&mut *conn)
            .await
            .map_err(db_err)?;
    match existing {
        Some(generation) => Ok(generation),
        None => store_experience_policy(conn, thresholds.0, thresholds.1, now).await,
    }
}

/// Outcome of [`recompute_experienced`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RecomputeOutcome {
    /// Rows whose `experienced` flag changed.
    pub changed: u64,
    /// Rows whose `counterparty_id` maps to no seller in `orders` (node key
    /// rotated, imported database). They keep their old value and old
    /// generation, which keeps them out of every experienced count.
    pub unresolved: u64,
}

/// Re-evaluate the whole `experienced` column under `(min_trades, min_days)`
/// in one transaction, together with the policy row (§10.7). Each snapshot is
/// re-evaluated at its own `last_success_at`, so the result depends only on
/// `orders`, the node key and the thresholds.
pub async fn recompute_experienced(
    pool: &Pool<Sqlite>,
    node_keys: &Keys,
    min_trades: u32,
    min_days: u32,
    now: i64,
) -> Result<RecomputeOutcome, MostroError> {
    let mut tx = pool.begin().await.map_err(db_err)?;
    let generation = store_experience_policy(&mut tx, min_trades, min_days, now).await?;

    // counterparty_id -> seller key. The ids are keyed hashes and cannot be
    // inverted, so the map is built forward from every seller key the node
    // has seen. `seller_pubkey` is included because the success hook falls
    // back to it when an order carries no master seller key.
    let sellers = sqlx::query_scalar::<_, String>(
        "SELECT master_seller_pubkey FROM orders WHERE master_seller_pubkey IS NOT NULL \
         UNION SELECT seller_pubkey FROM orders WHERE seller_pubkey IS NOT NULL",
    )
    .fetch_all(&mut *tx)
    .await
    .map_err(db_err)?;
    let by_id: HashMap<String, String> = sellers
        .into_iter()
        .map(|s| (counterparty_id(node_keys, &s), s))
        .collect();

    let rows = sqlx::query(
        "SELECT user_pubkey, payment_hash, counterparty_id, last_success_at, experienced \
           FROM payer_history_counterparties",
    )
    .fetch_all(&mut *tx)
    .await
    .map_err(db_err)?;

    let mut outcome = RecomputeOutcome::default();
    for row in rows {
        let user: String = row.get("user_pubkey");
        let hash: String = row.get("payment_hash");
        let cp: String = row.get("counterparty_id");
        let at: i64 = row.get("last_success_at");
        let old = row.get::<i64, _>("experienced") == 1;
        let Some(seller) = by_id.get(&cp) else {
            outcome.unresolved += 1;
            continue;
        };
        let exp = seller_experience(&mut tx, seller, &user, None, Some(at)).await?;
        let new = is_experienced(&exp, min_trades, min_days, at);
        sqlx::query(
            "UPDATE payer_history_counterparties SET experienced = ?1, policy_gen = ?2 \
              WHERE user_pubkey = ?3 AND payment_hash = ?4 AND counterparty_id = ?5",
        )
        .bind(i64::from(new))
        .bind(generation)
        .bind(&user)
        .bind(&hash)
        .bind(&cp)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        if new != old {
            outcome.changed += 1;
        }
    }
    tx.commit().await.map_err(db_err)?;
    Ok(outcome)
}

#[cfg(test)]
#[path = "db_tests.rs"]
mod tests;
