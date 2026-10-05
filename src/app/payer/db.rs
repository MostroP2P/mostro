//! Storage for payer declarations and payment-account history
//! (`docs/PAYER_HISTORY_ANTI_TRIANGULATION.md` §9, §10.1).
//!
//! Functions that must run inside the Success CAS transaction take a
//! `&mut SqliteConnection` (pass `&mut *tx`); the rest take the pool.
//! Nothing here hits LND or Nostr, and nothing here checks the feature flag:
//! callers gate (D-10).

use std::collections::HashMap;

use mostro_core::error::{MostroError, MostroError::MostroInternalErr, ServiceError};
use nostr_sdk::prelude::{Keys, PublicKey};
use sqlx::{AssertSqlSafe, Pool, Row, Sqlite, SqliteConnection};
use uuid::Uuid;

use super::{counterparty_id, is_experienced, payer_key, validate_payment_hash};
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

/// Insert or overwrite the declaration for `order_id` (last write wins),
/// with no status or buyer condition: a test fixture. Production writes go
/// through [`upsert_open_declaration`].
/// The row records the order's buyer trade key at that moment: a take that
/// rolls back to `pending` gives the order a new buyer later, and every read
/// below ignores a declaration made by a previous one.
#[cfg(test)]
pub async fn upsert_declaration(
    pool: &Pool<Sqlite>,
    order_id: Uuid,
    payment_hash: &str,
    now: i64,
) -> Result<(), MostroError> {
    sqlx::query(
        "INSERT INTO order_payer_declarations \
           (order_id, payment_hash, declared_at, buyer_pubkey) \
         VALUES (?1, ?2, ?3, (SELECT buyer_pubkey FROM orders WHERE id = ?1)) \
         ON CONFLICT(order_id) DO UPDATE SET payment_hash = excluded.payment_hash, \
                                             declared_at = excluded.declared_at, \
                                             buyer_pubkey = excluded.buyer_pubkey",
    )
    .bind(order_id)
    .bind(payment_hash)
    .bind(now)
    .execute(pool)
    .await
    .map_err(db_err)?;
    Ok(())
}

/// Order statuses in which a buyer may (re)declare its payer (§5.1).
pub const DECLARATION_OPEN_STATUSES: &str = "'waiting-payment','waiting-buyer-invoice','active'";

/// Upsert the declaration for `order_id` only while the order is still in
/// [`DECLARATION_OPEN_STATUSES`]. Status check and write are one statement,
/// so a declaration racing a concurrent `fiat-sent` cannot land after the
/// order froze, and only while `buyer_pubkey` and `seller_pubkey` (the
/// parties the handler read, `None` for no seller yet) are still the order's,
/// and while `taken_at` is still the value the handler read: a rollback
/// resets it and every take stamps a new one, so a request that raced a
/// rollback or a new take, even by the same two keys, cannot attach to it,
/// and the caller forwards to a seller the write confirmed. (`taken_at` is
/// also re-anchored when the hold invoice is paid; a request crossing that
/// is refused and the client re-sends it.) `taken_at` has second precision:
/// a cancel and a retake by the same two keys within one second, with this
/// buyer's own request in flight across both, would still match. That is the
/// same buyer declaring on its new take of the same order, and closing it
/// would need a take-generation column on `orders`, which §9 keeps to the
/// single `success_at` change. It
/// records that buyer trade key on the row. Returns `false` when nothing was
/// written.
pub async fn upsert_open_declaration(
    pool: &Pool<Sqlite>,
    order_id: Uuid,
    buyer_pubkey: &PublicKey,
    seller_pubkey: Option<&str>,
    taken_at: i64,
    payment_hash: &str,
    now: i64,
) -> Result<bool, MostroError> {
    let sql = format!(
        "INSERT INTO order_payer_declarations \
           (order_id, payment_hash, declared_at, buyer_pubkey) \
         SELECT ?1, ?2, ?3, o.buyer_pubkey FROM orders o \
          WHERE o.id = ?1 AND o.status IN ({DECLARATION_OPEN_STATUSES}) \
            AND o.buyer_pubkey = ?4 AND o.seller_pubkey IS ?5 AND o.taken_at = ?6 \
         ON CONFLICT(order_id) DO UPDATE SET payment_hash = excluded.payment_hash, \
                                             declared_at = excluded.declared_at, \
                                             buyer_pubkey = excluded.buyer_pubkey"
    );
    let done = sqlx::query(AssertSqlSafe(sql))
        .bind(order_id)
        .bind(payment_hash)
        .bind(now)
        .bind(buyer_pubkey.to_hex())
        .bind(seller_pubkey)
        .bind(taken_at)
        .execute(pool)
        .await
        .map_err(db_err)?;
    Ok(done.rows_affected() > 0)
}

/// The current declaration for `order_id`, if any: one made by the order's
/// current buyer.
pub async fn find_declaration(
    pool: &Pool<Sqlite>,
    order_id: Uuid,
) -> Result<Option<PayerDeclarationRow>, MostroError> {
    sqlx::query_as::<_, PayerDeclarationRow>(
        "SELECT d.order_id, d.payment_hash, d.declared_at \
           FROM order_payer_declarations d JOIN orders o ON o.id = d.order_id \
          WHERE d.order_id = ?1 AND d.buyer_pubkey = o.buyer_pubkey",
    )
    .bind(order_id)
    .fetch_optional(pool)
    .await
    .map_err(db_err)
}

/// The history snapshot frozen for `order_id`, if one was taken (§10.3).
pub async fn history_snapshot(
    pool: &Pool<Sqlite>,
    order_id: Uuid,
) -> Result<Option<mostro_core::payer::PaymentHistory>, MostroError> {
    let raw: Option<Option<String>> = sqlx::query_scalar(
        "SELECT history_snapshot FROM order_payer_declarations WHERE order_id = ?1",
    )
    .bind(order_id)
    .fetch_optional(pool)
    .await
    .map_err(db_err)?;
    raw.flatten()
        .map(|json| {
            serde_json::from_str(&json)
                .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))
        })
        .transpose()
}

/// Freeze `history` as the snapshot of `order_id` unless one is already
/// stored, and return the stored one: the first build wins, so concurrent
/// builds still agree.
pub async fn freeze_history_snapshot(
    pool: &Pool<Sqlite>,
    order_id: Uuid,
    history: &mostro_core::payer::PaymentHistory,
) -> Result<mostro_core::payer::PaymentHistory, MostroError> {
    let json = serde_json::to_string(history)
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
    sqlx::query(
        "UPDATE order_payer_declarations SET history_snapshot = ?2 \
          WHERE order_id = ?1 AND history_snapshot IS NULL",
    )
    .bind(order_id)
    .bind(json)
    .execute(pool)
    .await
    .map_err(db_err)?;
    Ok(history_snapshot(pool, order_id)
        .await?
        .unwrap_or_else(|| history.clone()))
}

/// Delete the declaration for `order_id` and return it when the order's
/// current buyer made it. The row can be taken exactly once, which makes it
/// the idempotency token of the success hook. A row left by a previous buyer
/// is deleted too, and reads as `None`.
pub async fn take_declaration(
    conn: &mut SqliteConnection,
    order_id: Uuid,
) -> Result<Option<PayerDeclarationRow>, MostroError> {
    let row = sqlx::query(
        "DELETE FROM order_payer_declarations WHERE order_id = ?1 \
         RETURNING order_id, payment_hash, declared_at, \
                   buyer_pubkey IS (SELECT buyer_pubkey FROM orders WHERE id = ?1) \
                     AND buyer_pubkey IS NOT NULL AS current_buyer",
    )
    .bind(order_id)
    .fetch_optional(conn)
    .await
    .map_err(db_err)?;
    Ok(row
        .filter(|r| r.get::<bool, _>("current_buyer"))
        .map(|r| PayerDeclarationRow {
            order_id: r.get("order_id"),
            payment_hash: r.get("payment_hash"),
            declared_at: r.get("declared_at"),
        }))
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

/// Counters for `(user_pubkey, payment_hash)`, looked up under the
/// [`payer_key`] of `payment_hash`; all zero when there is no history. A
/// row whose `policy_gen` is stale counts toward `distinct_counterparties` but never toward `experienced_counterparties`,
/// so a count is always evaluated under the advertised thresholds (§10.7);
/// one marked [`UNRESOLVED_POLICY_GENERATION`] counts toward neither.
pub async fn load_history(
    pool: &Pool<Sqlite>,
    node_keys: &Keys,
    user_pubkey: &str,
    payment_hash: &str,
) -> Result<HistoryCounters, MostroError> {
    // One statement, so the head row and the counterparty counts are read
    // from the same snapshot even while a success is being recorded.
    let row = sqlx::query(
        "SELECT h.successful_trades, h.first_success_at, h.last_success_at, \
                (SELECT COUNT(*) FROM payer_history_counterparties c \
                  WHERE c.user_pubkey = h.user_pubkey AND c.payer_key = h.payer_key \
                    AND c.policy_gen <> ?3) \
                  AS distinct_cp, \
                (SELECT COUNT(*) FROM payer_history_counterparties c \
                  WHERE c.user_pubkey = h.user_pubkey AND c.payer_key = h.payer_key \
                    AND c.experienced = 1 \
                    AND c.policy_gen = (SELECT generation FROM payer_history_policy WHERE id = 1)) \
                  AS experienced_cp \
           FROM payer_history h \
          WHERE h.user_pubkey = ?1 AND h.payer_key = ?2",
    )
    .bind(user_pubkey)
    .bind(payer_key(node_keys, payment_hash))
    .bind(UNRESOLVED_POLICY_GENERATION)
    .fetch_optional(pool)
    .await
    .map_err(db_err)?;
    let Some(row) = row else {
        return Ok(HistoryCounters::default());
    };
    Ok(HistoryCounters {
        successful_trades: to_u32(row.get::<i64, _>("successful_trades")),
        distinct_counterparties: to_u32(row.get::<i64, _>("distinct_cp")),
        experienced_counterparties: to_u32(row.get::<i64, _>("experienced_cp")),
        first_success_at: Some(row.get::<i64, _>("first_success_at")),
        last_success_at: Some(row.get::<i64, _>("last_success_at")),
    })
}

/// Record one successful trade for `(user_pubkey, payment_hash)` with the
/// counterparty `counterparty_id`. Stores the [`payer_key`] of
/// `payment_hash`, never the hash itself (D-2). Must run inside the Success CAS
/// transaction. Under a fixed policy generation the counterparty's
/// `experienced` flag only ever goes 0 → 1; a row left stale by §10.7 is
/// overwritten outright and rejoins the current generation.
// Every argument is a distinct column or key of the one upsert; a params
// struct would only rename them.
#[allow(clippy::too_many_arguments)]
pub async fn bump_history(
    conn: &mut SqliteConnection,
    node_keys: &Keys,
    user_pubkey: &str,
    payment_hash: &str,
    counterparty_id: &str,
    experienced: bool,
    policy_gen: i64,
    now: i64,
) -> Result<(), MostroError> {
    validate_payment_hash(payment_hash)?;
    let key = payer_key(node_keys, payment_hash);
    sqlx::query(
        "INSERT INTO payer_history \
           (user_pubkey, payer_key, first_success_at, last_success_at, successful_trades) \
         VALUES (?1, ?2, ?3, ?3, 1) \
         ON CONFLICT(user_pubkey, payer_key) \
         DO UPDATE SET first_success_at = MIN(first_success_at, excluded.first_success_at), \
                       last_success_at = MAX(last_success_at, excluded.last_success_at), \
                       successful_trades = successful_trades + 1",
    )
    .bind(user_pubkey)
    .bind(&key)
    .bind(now)
    .execute(&mut *conn)
    .await
    .map_err(db_err)?;
    sqlx::query(
        "INSERT INTO payer_history_counterparties \
           (user_pubkey, payer_key, counterparty_id, first_success_at, last_success_at, \
            experienced, policy_gen) \
         VALUES (?1, ?2, ?3, ?4, ?4, ?5, ?6) \
         ON CONFLICT(user_pubkey, payer_key, counterparty_id) \
         DO UPDATE SET first_success_at = MIN(first_success_at, excluded.first_success_at), \
                       last_success_at = MAX(last_success_at, excluded.last_success_at), \
                       experienced = CASE WHEN policy_gen = excluded.policy_gen \
                                          THEN MAX(experienced, excluded.experienced) \
                                          ELSE excluded.experienced END, \
                       policy_gen = excluded.policy_gen",
    )
    .bind(user_pubkey)
    .bind(&key)
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
///   `success_at` predates every snapshot and counts). Strictly before on
///   purpose: stamps have second resolution, so a success in the snapshot's
///   own second cannot be ordered against it and is left out. A recompute
///   can therefore under-count such a trade, never over-count one.
///
/// The age term dates each trade from its `success_at`, so an offer that
/// waited weeks before completing is not credited for the wait; legacy rows
/// with no stamp fall back to `created_at`.
pub async fn seller_experience(
    conn: &mut SqliteConnection,
    seller_master_pubkey: &str,
    buyer_pubkey: &str,
    current_order: Option<Uuid>,
    as_of: Option<i64>,
) -> Result<SellerExperience, MostroError> {
    let row = sqlx::query(
        "SELECT COUNT(*) AS n, MIN(COALESCE(success_at, created_at)) AS first_at \
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExperiencePolicy {
    pub generation: i64,
    pub min_trades: u32,
    pub min_days: u32,
    /// [`super::node_key_id`] of the node secret the last recompute resolved
    /// the counterparty ids with; `None` until a recompute ran. A different
    /// value means the stored ids no longer match the node (§10.7).
    pub node_key_id: Option<String>,
}

/// The stored policy, or `None` when the feature never ran on this database.
pub async fn load_experience_policy(
    pool: &Pool<Sqlite>,
) -> Result<Option<ExperiencePolicy>, MostroError> {
    let row = sqlx::query(
        "SELECT generation, experienced_min_trades, experienced_min_days, node_key_id \
           FROM payer_history_policy WHERE id = 1",
    )
    .fetch_optional(pool)
    .await
    .map_err(db_err)?;
    Ok(row.map(|r| ExperiencePolicy {
        generation: r.get("generation"),
        min_trades: to_u32(r.get("experienced_min_trades")),
        min_days: to_u32(r.get("experienced_min_days")),
        node_key_id: r.get("node_key_id"),
    }))
}

/// Record `(min_trades, min_days)`, evaluated under the node secret whose
/// [`super::node_key_id`] is `node_key_id`, as the current policy, bumping the generation (1 on
/// first use). Returns the new generation.
pub async fn store_experience_policy(
    conn: &mut SqliteConnection,
    node_key_id: &str,
    min_trades: u32,
    min_days: u32,
    now: i64,
) -> Result<i64, MostroError> {
    sqlx::query_scalar::<_, i64>(
        "INSERT INTO payer_history_policy \
           (id, generation, experienced_min_trades, experienced_min_days, evaluated_at, \
            node_key_id) \
         VALUES (1, 1, ?1, ?2, ?3, ?4) \
         ON CONFLICT(id) DO UPDATE SET generation = generation + 1, \
                                       experienced_min_trades = excluded.experienced_min_trades, \
                                       experienced_min_days = excluded.experienced_min_days, \
                                       evaluated_at = excluded.evaluated_at, \
                                       node_key_id = excluded.node_key_id \
         RETURNING generation",
    )
    .bind(i64::from(min_trades))
    .bind(i64::from(min_days))
    .bind(now)
    .bind(node_key_id)
    .fetch_one(conn)
    .await
    .map_err(db_err)
}

/// Generation the recompute stamps on a snapshot whose `counterparty_id`
/// resolves to no seller under the current node key. Such a row cannot be
/// matched to the seller again (a later trade with that seller gets a new
/// id), so it leaves the distinct count too, instead of counting the same
/// seller twice. Its trades still count in `successful_trades`.
pub const UNRESOLVED_POLICY_GENERATION: i64 = -1;

/// Generation stamped on a snapshot evaluated under thresholds the stored
/// policy does not hold yet. Real generations start at 1, so such a row
/// never counts as experienced until the recompute (§10.7) re-evaluates it.
pub const UNRECOMPUTED_POLICY_GENERATION: i64 = 0;

/// Generation to stamp into a snapshot taken right now (§10.5). Seeds the
/// policy row with `thresholds` on first use, so the success path never
/// races the boot-time recompute. When the stored policy holds other
/// thresholds (they changed and the recompute has not run), returns
/// [`UNRECOMPUTED_POLICY_GENERATION`] instead of mixing the two policies
/// under one generation.
///
/// The seed records the [`super::node_key_id`] of `node_keys`, the secret the
/// rows written next are keyed with, so a later change of secret is always
/// detected (§10.7) and never hides behind an unknown one.
pub async fn current_policy_generation(
    conn: &mut SqliteConnection,
    node_keys: &Keys,
    thresholds: (u32, u32),
    now: i64,
) -> Result<i64, MostroError> {
    // Insert-if-absent first, then read: a write statement up front takes
    // the write lock directly instead of upgrading a read inside the
    // caller's transaction, which SQLite can refuse with SQLITE_BUSY.
    sqlx::query(
        "INSERT INTO payer_history_policy \
           (id, generation, experienced_min_trades, experienced_min_days, evaluated_at, \
            node_key_id) \
         VALUES (1, 1, ?1, ?2, ?3, ?4) \
         ON CONFLICT(id) DO NOTHING",
    )
    .bind(i64::from(thresholds.0))
    .bind(i64::from(thresholds.1))
    .bind(now)
    .bind(super::node_key_id(node_keys))
    .execute(&mut *conn)
    .await
    .map_err(db_err)?;
    sqlx::query_scalar::<_, i64>(
        "SELECT CASE WHEN experienced_min_trades = ?1 AND experienced_min_days = ?2 \
                     THEN generation ELSE ?3 END \
         FROM payer_history_policy WHERE id = 1",
    )
    .bind(i64::from(thresholds.0))
    .bind(i64::from(thresholds.1))
    .bind(UNRECOMPUTED_POLICY_GENERATION)
    .fetch_one(conn)
    .await
    .map_err(db_err)
}

/// Outcome of [`recompute_experienced`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RecomputeOutcome {
    /// Rows whose `experienced` flag changed.
    pub changed: u64,
    /// Rows whose `counterparty_id` maps to no seller in `orders` (an
    /// imported database; a rotated node key discards the rows instead).
    /// They keep their value and are stamped
    /// [`UNRESOLVED_POLICY_GENERATION`], which keeps them out of both the
    /// experienced and the distinct counts.
    pub unresolved: u64,
    /// `payer_history` rows dropped because the stored policy was evaluated
    /// under another node secret (§10.7). Zero unless the secret changed.
    pub discarded: u64,
}

/// Re-evaluate the whole `experienced` column under `(min_trades, min_days)`
/// in one transaction, together with the policy row (§10.7). The caller
/// logs [`RecomputeOutcome::unresolved`] at `warn` (never the rows), since on
/// a healthy node it is zero. Each snapshot is
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
    let node_key_id = super::node_key_id(node_keys);
    let discarded = discard_history_of_another_node_key(&mut tx, &node_key_id).await?;
    let generation =
        store_experience_policy(&mut tx, &node_key_id, min_trades, min_days, now).await?;

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
        "SELECT user_pubkey, payer_key, counterparty_id, last_success_at, experienced \
           FROM payer_history_counterparties",
    )
    .fetch_all(&mut *tx)
    .await
    .map_err(db_err)?;

    let mut outcome = RecomputeOutcome {
        discarded,
        ..RecomputeOutcome::default()
    };
    for row in rows {
        let user: String = row.get("user_pubkey");
        let hash: String = row.get("payer_key");
        let cp: String = row.get("counterparty_id");
        let at: i64 = row.get("last_success_at");
        let old = row.get::<i64, _>("experienced") == 1;
        let Some(seller) = by_id.get(&cp) else {
            sqlx::query(
                "UPDATE payer_history_counterparties SET policy_gen = ?1 \
                  WHERE user_pubkey = ?2 AND payer_key = ?3 AND counterparty_id = ?4",
            )
            .bind(UNRESOLVED_POLICY_GENERATION)
            .bind(&user)
            .bind(&hash)
            .bind(&cp)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
            outcome.unresolved += 1;
            continue;
        };
        let exp = seller_experience(&mut tx, seller, &user, None, Some(at)).await?;
        let new = is_experienced(&exp, min_trades, min_days, at);
        sqlx::query(
            "UPDATE payer_history_counterparties SET experienced = ?1, policy_gen = ?2 \
              WHERE user_pubkey = ?3 AND payer_key = ?4 AND counterparty_id = ?5",
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
    // Frozen payment-history snapshots (§10.3) hold counts taken under the
    // previous policy; drop them so the next query re-takes them under the
    // one the info event now advertises.
    sqlx::query("UPDATE order_payer_declarations SET history_snapshot = NULL")
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
    tx.commit().await.map_err(db_err)?;
    Ok(outcome)
}

/// Drop every history row when the stored policy was evaluated under a node
/// secret other than `node_key_id`. Rows are keyed by that secret
/// ([`payer_key`]): under a new one no declaration can reach them again, and
/// keeping them would only keep data nobody can use. Every seed records the
/// key, so a policy row without one predates that and proves nothing either
/// way; it keeps the rows. Returns the number of
/// `payer_history` rows dropped.
async fn discard_history_of_another_node_key(
    conn: &mut SqliteConnection,
    node_key_id: &str,
) -> Result<u64, MostroError> {
    let stored: Option<Option<String>> =
        sqlx::query_scalar("SELECT node_key_id FROM payer_history_policy WHERE id = 1")
            .fetch_optional(&mut *conn)
            .await
            .map_err(db_err)?;
    let Some(Some(stored)) = stored else {
        return Ok(0);
    };
    if stored == node_key_id {
        return Ok(0);
    }
    sqlx::query("DELETE FROM payer_history_counterparties")
        .execute(&mut *conn)
        .await
        .map_err(db_err)?;
    let deleted = sqlx::query("DELETE FROM payer_history")
        .execute(&mut *conn)
        .await
        .map_err(db_err)?;
    Ok(deleted.rows_affected())
}

#[cfg(test)]
#[path = "db_tests.rs"]
mod tests;
