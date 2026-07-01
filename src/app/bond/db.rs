//! Database helpers for the `bonds` table.
//!
//! Phase 0 exposes the CRUD surface later phases will need. Nothing in
//! this module hits LND or the Nostr client — it's purely storage.

use mostro_core::db::Crud;
use mostro_core::error::{MostroError, MostroError::MostroInternalErr, ServiceError};
use mostro_core::order::{Order, Status};
use sqlx::{AssertSqlSafe, Pool, Sqlite};
use uuid::Uuid;

use super::model::Bond;
use super::types::{BondRole, BondState};

/// Defensive upper bound on a `range_parent_id` walk. The real chain
/// length is bounded by how many slices a range can be split into
/// (`max_amount / min_amount`), always small; this cap exists only so a
/// corrupt cycle in the DB can never hang the daemon.
const MAX_RANGE_CHAIN_DEPTH: usize = 1024;

/// Insert a new bond row. Returns the persisted `Bond`.
pub async fn create_bond(
    pool: &Pool<Sqlite>,
    bond: Bond,
) -> Result<Bond, mostro_core::error::MostroError> {
    bond.create(pool)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))
}

/// Look up the parent bond row for a given order + role. Returns `None`
/// if no bond exists for that pair (the normal case when the feature is
/// off or doesn't apply to the role).
///
/// Phase 6 introduces child slash rows that share the parent's `order_id`
/// and `role`; those rows carry a non-NULL `parent_bond_id`. The
/// `parent_bond_id IS NULL` predicate keeps this lookup pinned to the
/// parent bond so state transitions always target the right row.
pub async fn find_bond_by_order_and_role(
    pool: &Pool<Sqlite>,
    order_id: Uuid,
    role: BondRole,
) -> Result<Option<Bond>, mostro_core::error::MostroError> {
    let role_str = role.to_string();
    sqlx::query_as::<_, Bond>(
        "SELECT * FROM bonds \
         WHERE order_id = ? AND role = ? AND parent_bond_id IS NULL \
         LIMIT 1",
    )
    .bind(order_id)
    .bind(role_str)
    .fetch_optional(pool)
    .await
    .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))
}

/// List every bond currently in the given state. Used by the Phase 3
/// payout scheduler.
pub async fn find_bonds_by_state(
    pool: &Pool<Sqlite>,
    state: BondState,
) -> Result<Vec<Bond>, mostro_core::error::MostroError> {
    let state_str = state.to_string();
    sqlx::query_as::<_, Bond>("SELECT * FROM bonds WHERE state = ? ORDER BY created_at ASC")
        .bind(state_str)
        .fetch_all(pool)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))
}

/// Look up a bond row by its primary key. Used by the Phase 3 payout
/// scheduler to check a child slash row's parent state (skip while the
/// parent HTLC is still `Locked`, i.e. before range close).
pub async fn find_bond_by_id(
    pool: &Pool<Sqlite>,
    id: Uuid,
) -> Result<Option<Bond>, mostro_core::error::MostroError> {
    Bond::by_id(pool, id)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))
}

/// Look up a bond row by its Lightning payment hash. The hash uniquely
/// identifies the bond hold invoice, so this is what the LND subscriber
/// uses to correlate incoming invoice events back to a `Bond`.
pub async fn find_bond_by_hash(
    pool: &Pool<Sqlite>,
    hash: &str,
) -> Result<Option<Bond>, mostro_core::error::MostroError> {
    sqlx::query_as::<_, Bond>("SELECT * FROM bonds WHERE hash = ? LIMIT 1")
        .bind(hash)
        .fetch_optional(pool)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))
}

/// List every bond row that still has an outstanding LND HTLC, i.e. is in
/// `Requested` or `Locked`. Used on daemon startup to resubscribe to
/// in-flight bond hold invoices, and as the Phase 1 workhorse for the
/// "always release" exits — we filter further on `order_id` in
/// [`find_active_bonds_for_order`].
pub async fn find_active_bonds(
    pool: &Pool<Sqlite>,
) -> Result<Vec<Bond>, mostro_core::error::MostroError> {
    let requested = BondState::Requested.to_string();
    let locked = BondState::Locked.to_string();
    sqlx::query_as::<_, Bond>("SELECT * FROM bonds WHERE state IN (?, ?) ORDER BY created_at ASC")
        .bind(requested)
        .bind(locked)
        .fetch_all(pool)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))
}

/// List the still-outstanding bonds attached to a single order. Phase 1
/// uses this to release every bond on any order exit path (cancel,
/// release, admin actions, scheduler timeouts).
pub async fn find_active_bonds_for_order(
    pool: &Pool<Sqlite>,
    order_id: Uuid,
) -> Result<Vec<Bond>, mostro_core::error::MostroError> {
    let requested = BondState::Requested.to_string();
    let locked = BondState::Locked.to_string();
    sqlx::query_as::<_, Bond>(
        "SELECT * FROM bonds \
         WHERE order_id = ? AND state IN (?, ?) \
         ORDER BY created_at ASC",
    )
    .bind(order_id)
    .bind(requested)
    .bind(locked)
    .fetch_all(pool)
    .await
    .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))
}

/// Look up the active (`Requested` or `Locked`) bond row for a given
/// `(order_id, taker_pubkey)` pair. Used by the take handlers'
/// idempotent-retry check (a taker re-emitting `take-buy` / `take-sell`
/// while their bond is still `Requested` must get back the same
/// `payment_request`, not a fresh row) and by `cancel_order_by_taker`
/// to scope the cancel to the sender's own bond under concurrent
/// taker bonds.
///
/// Filters on `parent_bond_id IS NULL` to ignore Phase 6 child slash
/// rows, mirroring `find_bond_by_order_and_role`.
pub async fn find_active_bond_by_taker(
    pool: &Pool<Sqlite>,
    order_id: Uuid,
    taker_pubkey: &str,
) -> Result<Option<Bond>, mostro_core::error::MostroError> {
    let requested = BondState::Requested.to_string();
    let locked = BondState::Locked.to_string();
    sqlx::query_as::<_, Bond>(
        "SELECT * FROM bonds \
         WHERE order_id = ? AND pubkey = ? AND state IN (?, ?) \
           AND parent_bond_id IS NULL \
         ORDER BY created_at ASC \
         LIMIT 1",
    )
    .bind(order_id)
    .bind(taker_pubkey)
    .bind(requested)
    .bind(locked)
    .fetch_optional(pool)
    .await
    .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))
}

/// Walk the `range_parent_id` chain from `order` up to the range root —
/// the order that owns the maker bond (Phase 6).
///
/// `range_parent_id` is a linked list to the *immediate* parent slice, not
/// a star to the root (see `create_base_order` in `app::release`), so the
/// maker bond lives on whichever order has `range_parent_id IS NULL`. A
/// non-range order or an already-root order returns itself. The walk is
/// bounded by [`MAX_RANGE_CHAIN_DEPTH`]; a missing parent row (should never
/// happen) terminates the walk at the deepest order we could load.
pub async fn find_range_root_order(
    pool: &Pool<Sqlite>,
    order: Order,
) -> Result<Order, MostroError> {
    let mut current = order;
    for _ in 0..MAX_RANGE_CHAIN_DEPTH {
        let Some(parent_id) = current.range_parent_id else {
            return Ok(current);
        };
        match Order::by_id(pool, parent_id)
            .await
            .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?
        {
            Some(parent) => current = parent,
            None => return Ok(current),
        }
    }
    Err(MostroInternalErr(ServiceError::UnexpectedError(
        "range_parent_id chain exceeded max depth (possible cycle)".to_string(),
    )))
}

/// Find the parent **maker** bond governing `order`, walking the range
/// chain to its root first.
///
/// A maker slash (or release) can land on any slice in a range chain, but
/// there is only ever one maker bond and it lives on the root order. This
/// resolves that single bond from any slice. Returns `None` when no maker
/// bond exists (feature off, `apply_to` excludes the maker, or the bond
/// was already released).
pub async fn find_maker_bond_for_order(
    pool: &Pool<Sqlite>,
    order: &Order,
) -> Result<Option<Bond>, MostroError> {
    let root = find_range_root_order(pool, order.clone()).await?;
    find_bond_by_order_and_role(pool, root.id, BondRole::Maker).await
}

/// Every child slash row that belongs to `parent_bond_id` (Phase 6
/// range-order accounting). Ordered oldest-first for deterministic
/// iteration at parent-close.
pub async fn find_child_slashes_for_parent(
    pool: &Pool<Sqlite>,
    parent_bond_id: Uuid,
) -> Result<Vec<Bond>, MostroError> {
    sqlx::query_as::<_, Bond>(
        "SELECT * FROM bonds WHERE parent_bond_id = ? ORDER BY created_at ASC",
    )
    .bind(parent_bond_id)
    .fetch_all(pool)
    .await
    .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))
}

/// Order statuses from which a range slice can never re-activate. Once
/// every order in a range tree has reached one of these, the maker bond is
/// safe to close — no descendant can still draw against it. Kept as the
/// complement of the in-flight states (`Pending`, `Active`, the `Waiting*`
/// states, `FiatSent`, `SettledHoldInvoice`, `Dispute`, `InProgress`) so a
/// new in-flight status defaults to "not terminal" (conservative: the sweep
/// won't prematurely close a bond it doesn't understand).
const TERMINAL_ORDER_STATUSES: [Status; 7] = [
    Status::Success,
    Status::Canceled,
    Status::CanceledByAdmin,
    Status::SettledByAdmin,
    Status::CompletedByAdmin,
    Status::CooperativelyCanceled,
    Status::Expired,
];

/// Every `Locked` **parent** maker bond (`role = 'maker'`,
/// `parent_bond_id IS NULL`). The reconciliation sweep
/// (`reconcile_stranded_range_maker_bonds`) starts from this set and then
/// filters to the ones whose whole range tree has terminated. Child slash
/// rows and refund rows (both `parent_bond_id IS NOT NULL`) are excluded.
pub async fn find_locked_maker_parent_bonds(pool: &Pool<Sqlite>) -> Result<Vec<Bond>, MostroError> {
    let locked = BondState::Locked.to_string();
    let maker = BondRole::Maker.to_string();
    sqlx::query_as::<_, Bond>(
        "SELECT * FROM bonds \
         WHERE state = ? AND role = ? AND parent_bond_id IS NULL \
         ORDER BY created_at ASC",
    )
    .bind(locked)
    .bind(maker)
    .fetch_all(pool)
    .await
    .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))
}

/// Is the whole range tree rooted at `root_id` fully terminal — i.e. does no
/// order reachable from the root via `range_parent_id` sit in a non-terminal
/// status?
///
/// Walks the `range_parent_id` linked list **downwards** with a recursive
/// CTE (the inverse of [`find_range_root_order`], which walks up). Returns
/// `true` only when every order in the tree — the root included — is in a
/// [`TERMINAL_ORDER_STATUSES`] state, so the maker bond can be safely closed.
/// A non-range or already-root order tree is just the single root row.
pub async fn range_tree_fully_terminal(
    pool: &Pool<Sqlite>,
    root_id: Uuid,
) -> Result<bool, MostroError> {
    let placeholders = TERMINAL_ORDER_STATUSES
        .iter()
        .map(|_| "?")
        .collect::<Vec<_>>()
        .join(", ");
    // `UNION` (not `UNION ALL`) is deliberate: it deduplicates, so a corrupt
    // `range_parent_id` cycle (e.g. A↔B) terminates the recursion instead of
    // looping unbounded and hanging the scheduler tick. This mirrors the
    // `MAX_RANGE_CHAIN_DEPTH` guard on the upward walk in
    // `find_range_root_order`. Dedup is safe here because the result is only
    // reduced to `COUNT(*) … == 0` below — the exact count is never used.
    let sql = format!(
        "WITH RECURSIVE tree(id, status) AS ( \
             SELECT id, status FROM orders WHERE id = ? \
             UNION \
             SELECT o.id, o.status FROM orders o JOIN tree t ON o.range_parent_id = t.id \
         ) \
         SELECT COUNT(*) FROM tree WHERE status NOT IN ({placeholders})"
    );
    let mut query = sqlx::query_scalar::<_, i64>(AssertSqlSafe(sql)).bind(root_id);
    for status in TERMINAL_ORDER_STATUSES {
        query = query.bind(status.to_string());
    }
    let non_terminal = query
        .fetch_one(pool)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
    Ok(non_terminal == 0)
}

/// Update a bond row by primary key. Returns the persisted `Bond`.
pub async fn update_bond(
    pool: &Pool<Sqlite>,
    bond: Bond,
) -> Result<Bond, mostro_core::error::MostroError> {
    bond.update(pool)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::bond::crud::BOND_INSERT_COLUMNS;
    use crate::app::bond::model::Bond;
    use crate::app::bond::types::{BondRole, BondSlashReason, BondState};
    use sqlx::sqlite::SqlitePoolOptions;

    async fn setup_pool() -> Pool<Sqlite> {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect(":memory:")
            .await
            .expect("open in-memory sqlite");
        // Minimal orders table: bonds has an FK on it.
        sqlx::query(include_str!(
            "../../../migrations/20221222153301_orders.sql"
        ))
        .execute(&pool)
        .await
        .expect("orders migration");
        sqlx::query(include_str!(
            "../../../migrations/20260423120000_anti_abuse_bond.sql"
        ))
        .execute(&pool)
        .await
        .expect("bonds migration");
        sqlx::query(include_str!(
            "../../../migrations/20260518120000_bond_payout_payment_hash.sql"
        ))
        .execute(&pool)
        .await
        .expect("bond_payout_payment_hash migration");
        sqlx::query(include_str!(
            "../../../migrations/20260611120000_bond_slice_slash_unique.sql"
        ))
        .execute(&pool)
        .await
        .expect("bond_slice_slash_unique migration");
        // SQLite doesn't enforce FKs unless asked. Turn them on so the FK to
        // `orders` is a real constraint in tests (mirrors production).
        sqlx::query("PRAGMA foreign_keys = ON")
            .execute(&pool)
            .await
            .expect("enable fk");
        pool
    }

    async fn insert_parent_order(pool: &Pool<Sqlite>, id: Uuid) {
        sqlx::query(
            r#"INSERT INTO orders (
                id, kind, event_id, status, premium, payment_method,
                amount, fiat_code, fiat_amount, created_at, expires_at
            ) VALUES (?, 'buy', ?, 'pending', 0, 'ln', 1000, 'USD', 10, 0, 0)"#,
        )
        .bind(id)
        .bind(id.simple().to_string())
        .execute(pool)
        .await
        .expect("insert parent order");
    }

    /// Insert an order with an explicit status and optional `range_parent_id`,
    /// so the range-tree-terminal walk can be exercised over a real chain.
    async fn insert_order_with(
        pool: &Pool<Sqlite>,
        id: Uuid,
        status: Status,
        range_parent_id: Option<Uuid>,
    ) {
        sqlx::query(
            r#"INSERT INTO orders (
                id, kind, event_id, status, premium, payment_method,
                amount, fiat_code, fiat_amount, range_parent_id, created_at, expires_at
            ) VALUES (?, 'sell', ?, ?, 0, 'ln', 1000, 'USD', 10, ?, 0, 0)"#,
        )
        .bind(id)
        .bind(id.simple().to_string())
        .bind(status.to_string())
        .bind(range_parent_id)
        .execute(pool)
        .await
        .expect("insert order with status");
    }

    fn dummy_bond(order_id: Uuid, role: BondRole) -> Bond {
        Bond::new_requested(order_id, "a".repeat(64), role, 1_500)
    }

    /// Bond with a unique scalar sentinel per INSERT column so
    /// `bond_insert_column_bind_alignment` can detect positional drift.
    fn sentinel_bond_for_insert(order_id: Uuid) -> Bond {
        Bond {
            id: Uuid::parse_str("00000000-0000-4000-8000-000000000003").unwrap(),
            order_id,
            parent_bond_id: Some(Uuid::parse_str("00000000-0000-4000-8000-000000000001").unwrap()),
            child_order_id: Some(Uuid::parse_str("00000000-0000-4000-8000-000000000002").unwrap()),
            pubkey: "5".repeat(64),
            role: BondRole::Taker.to_string(),
            amount_sats: 7,
            slashed_share_sats: 8,
            state: BondState::Locked.to_string(),
            slashed_reason: Some(BondSlashReason::LostDispute.to_string()),
            hash: Some("h".repeat(64)),
            preimage: Some("p".repeat(64)),
            payment_request: Some("pr-sentinel".into()),
            payout_invoice: Some("pi-sentinel".into()),
            payout_routing_fee_sats: Some(15),
            payout_payment_hash: Some("q".repeat(64)),
            node_share_sats: Some(17),
            payout_attempts: 18,
            invoice_request_attempts: 19,
            last_invoice_request_at: Some(20),
            locked_at: Some(21),
            released_at: Some(22),
            slashed_at: Some(23),
            created_at: 24,
            taker_identity: Some("t".repeat(64)),
            taker_trade_index: Some(26),
            taker_invoice: Some("ti-sentinel".into()),
            taker_fiat_amount: Some(28),
            taker_amount: Some(29),
            taker_fee: Some(30),
            taker_dev_fee: Some(31),
        }
    }

    fn expected_db_text(column: &str, bond: &Bond) -> Option<String> {
        match column {
            "id" => Some(bond.id.to_string()),
            "order_id" => Some(bond.order_id.to_string()),
            "parent_bond_id" => bond.parent_bond_id.map(|id| id.to_string()),
            "child_order_id" => bond.child_order_id.map(|id| id.to_string()),
            "pubkey" => Some(bond.pubkey.clone()),
            "role" => Some(bond.role.clone()),
            "amount_sats" => Some(bond.amount_sats.to_string()),
            "slashed_share_sats" => Some(bond.slashed_share_sats.to_string()),
            "state" => Some(bond.state.clone()),
            "slashed_reason" => bond.slashed_reason.clone(),
            "hash" => bond.hash.clone(),
            "preimage" => bond.preimage.clone(),
            "payment_request" => bond.payment_request.clone(),
            "payout_invoice" => bond.payout_invoice.clone(),
            "payout_routing_fee_sats" => bond.payout_routing_fee_sats.map(|v| v.to_string()),
            "payout_payment_hash" => bond.payout_payment_hash.clone(),
            "node_share_sats" => bond.node_share_sats.map(|v| v.to_string()),
            "payout_attempts" => Some(bond.payout_attempts.to_string()),
            "invoice_request_attempts" => Some(bond.invoice_request_attempts.to_string()),
            "last_invoice_request_at" => bond.last_invoice_request_at.map(|v| v.to_string()),
            "locked_at" => bond.locked_at.map(|v| v.to_string()),
            "released_at" => bond.released_at.map(|v| v.to_string()),
            "slashed_at" => bond.slashed_at.map(|v| v.to_string()),
            "created_at" => Some(bond.created_at.to_string()),
            "taker_identity" => bond.taker_identity.clone(),
            "taker_trade_index" => bond.taker_trade_index.map(|v| v.to_string()),
            "taker_invoice" => bond.taker_invoice.clone(),
            "taker_fiat_amount" => bond.taker_fiat_amount.map(|v| v.to_string()),
            "taker_amount" => bond.taker_amount.map(|v| v.to_string()),
            "taker_fee" => bond.taker_fee.map(|v| v.to_string()),
            "taker_dev_fee" => bond.taker_dev_fee.map(|v| v.to_string()),
            other => panic!("BOND_INSERT_COLUMNS entry {other:?} has no sentinel expectation"),
        }
    }

    fn row_text(row: &sqlx::sqlite::SqliteRow, column: &str) -> Option<String> {
        use sqlx::Row;

        match column {
            "id" | "order_id" => Some(row.try_get::<Uuid, _>(column).unwrap().to_string()),
            "parent_bond_id" | "child_order_id" => row
                .try_get::<Option<Uuid>, _>(column)
                .unwrap()
                .map(|id| id.to_string()),
            "pubkey" | "role" | "state" => Some(row.try_get::<String, _>(column).unwrap()),
            "amount_sats"
            | "slashed_share_sats"
            | "payout_attempts"
            | "invoice_request_attempts"
            | "created_at" => Some(row.try_get::<i64, _>(column).unwrap().to_string()),
            "slashed_reason"
            | "hash"
            | "preimage"
            | "payment_request"
            | "payout_invoice"
            | "payout_payment_hash"
            | "taker_identity"
            | "taker_invoice" => row.try_get::<Option<String>, _>(column).unwrap(),
            "payout_routing_fee_sats"
            | "node_share_sats"
            | "last_invoice_request_at"
            | "locked_at"
            | "released_at"
            | "slashed_at"
            | "taker_trade_index"
            | "taker_fiat_amount"
            | "taker_amount"
            | "taker_fee"
            | "taker_dev_fee" => row
                .try_get::<Option<i64>, _>(column)
                .unwrap()
                .map(|v| v.to_string()),
            other => panic!("BOND_INSERT_COLUMNS entry {other:?} has no row decoder"),
        }
    }

    #[tokio::test]
    async fn bond_insert_column_bind_alignment() {
        let pool = setup_pool().await;
        let order_id = Uuid::new_v4();
        insert_parent_order(&pool, order_id).await;

        let bond = sentinel_bond_for_insert(order_id);
        let bond_id = bond.id;
        create_bond(&pool, bond.clone()).await.unwrap();

        let row = sqlx::query("SELECT * FROM bonds WHERE id = ?")
            .bind(bond_id)
            .fetch_one(&pool)
            .await
            .expect("fetch bond row");

        for &column in BOND_INSERT_COLUMNS {
            let expected = expected_db_text(column, &bond);
            let actual = row_text(&row, column);
            assert_eq!(actual, expected, "column {column}");
        }
    }

    #[tokio::test]
    async fn range_tree_terminal_walks_descendants() {
        // root <- child (range_parent_id chain). The tree is "fully terminal"
        // only when BOTH the root and every descendant are terminal.
        let pool = setup_pool().await;
        let root = Uuid::new_v4();
        let child = Uuid::new_v4();
        insert_order_with(&pool, root, Status::CooperativelyCanceled, None).await;
        insert_order_with(&pool, child, Status::Active, Some(root)).await;

        // A still-Active descendant keeps the tree non-terminal.
        assert!(
            !range_tree_fully_terminal(&pool, root).await.unwrap(),
            "an Active descendant must make the tree non-terminal"
        );

        // Terminate the descendant → the whole tree is terminal.
        sqlx::query("UPDATE orders SET status = ? WHERE id = ?")
            .bind(Status::Expired.to_string())
            .bind(child)
            .execute(&pool)
            .await
            .unwrap();
        assert!(
            range_tree_fully_terminal(&pool, root).await.unwrap(),
            "root + descendant both terminal → tree terminal"
        );

        // A non-terminal root alone also blocks (single-row tree).
        let lone = Uuid::new_v4();
        insert_order_with(&pool, lone, Status::Pending, None).await;
        assert!(!range_tree_fully_terminal(&pool, lone).await.unwrap());
    }

    #[tokio::test]
    async fn range_tree_terminal_is_cycle_safe() {
        // A corrupt `range_parent_id` cycle (A↔B) must NOT hang the query:
        // the CTE uses `UNION` (dedup), so the walk terminates. This test
        // completing at all proves there is no unbounded loop.
        let pool = setup_pool().await;
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        // Insert both first (no parent), then point them at each other.
        insert_order_with(&pool, a, Status::Active, None).await;
        insert_order_with(&pool, b, Status::Active, None).await;
        sqlx::query("UPDATE orders SET range_parent_id = ? WHERE id = ?")
            .bind(b)
            .bind(a)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("UPDATE orders SET range_parent_id = ? WHERE id = ?")
            .bind(a)
            .bind(b)
            .execute(&pool)
            .await
            .unwrap();

        // Returns promptly without error; both nodes Active → non-terminal.
        assert!(!range_tree_fully_terminal(&pool, a).await.unwrap());

        // Terminate both → the cyclic tree reads as terminal (still no hang).
        sqlx::query("UPDATE orders SET status = ?")
            .bind(Status::Expired.to_string())
            .execute(&pool)
            .await
            .unwrap();
        assert!(range_tree_fully_terminal(&pool, a).await.unwrap());
    }

    #[tokio::test]
    async fn locked_maker_parent_bonds_excludes_children() {
        let pool = setup_pool().await;
        let order_id = Uuid::new_v4();
        let child_order_id = Uuid::new_v4();
        insert_parent_order(&pool, order_id).await;
        insert_parent_order(&pool, child_order_id).await;

        // A Locked maker parent.
        let mut parent = dummy_bond(order_id, BondRole::Maker);
        parent.state = BondState::Locked.to_string();
        let parent = create_bond(&pool, parent).await.unwrap();

        // A child slash row (PendingPayout) — must be excluded.
        let mut child = dummy_bond(order_id, BondRole::Maker);
        child.parent_bond_id = Some(parent.id);
        child.child_order_id = Some(child_order_id);
        child.state = BondState::PendingPayout.to_string();
        create_bond(&pool, child).await.unwrap();

        // A Locked taker bond — wrong role, excluded.
        let mut taker = dummy_bond(child_order_id, BondRole::Taker);
        taker.state = BondState::Locked.to_string();
        create_bond(&pool, taker).await.unwrap();

        let locked = find_locked_maker_parent_bonds(&pool).await.unwrap();
        assert_eq!(locked.len(), 1);
        assert_eq!(locked[0].id, parent.id);
    }

    #[tokio::test]
    async fn insert_and_fetch_by_order_and_role() {
        let pool = setup_pool().await;
        let order_id = Uuid::new_v4();
        insert_parent_order(&pool, order_id).await;
        let created = create_bond(&pool, dummy_bond(order_id, BondRole::Taker))
            .await
            .expect("insert");
        let fetched = find_bond_by_order_and_role(&pool, order_id, BondRole::Taker)
            .await
            .expect("query")
            .expect("row present");
        assert_eq!(fetched.id, created.id);
        assert_eq!(fetched.role, "taker");
    }

    #[tokio::test]
    async fn fetch_by_order_and_role_ignores_child_rows() {
        // Phase 6 will store child slash rows that share the parent bond's
        // `order_id` and `role`; the lookup must still return the parent.
        let pool = setup_pool().await;
        let order_id = Uuid::new_v4();
        let child_order_id = Uuid::new_v4();
        insert_parent_order(&pool, order_id).await;
        insert_parent_order(&pool, child_order_id).await;

        let parent = create_bond(&pool, dummy_bond(order_id, BondRole::Maker))
            .await
            .expect("insert parent");

        let mut child = dummy_bond(order_id, BondRole::Maker);
        child.parent_bond_id = Some(parent.id);
        child.child_order_id = Some(child_order_id);
        create_bond(&pool, child).await.expect("insert child");

        let fetched = find_bond_by_order_and_role(&pool, order_id, BondRole::Maker)
            .await
            .expect("query")
            .expect("row present");
        assert_eq!(fetched.id, parent.id);
        assert!(fetched.parent_bond_id.is_none());
    }

    #[tokio::test]
    async fn fetch_missing_returns_none() {
        let pool = setup_pool().await;
        let res = find_bond_by_order_and_role(&pool, Uuid::new_v4(), BondRole::Taker)
            .await
            .expect("query");
        assert!(res.is_none());
    }

    #[tokio::test]
    async fn find_by_hash_returns_match() {
        let pool = setup_pool().await;
        let order_id = Uuid::new_v4();
        insert_parent_order(&pool, order_id).await;

        let mut bond = dummy_bond(order_id, BondRole::Taker);
        bond.hash = Some("c".repeat(64));
        let created = create_bond(&pool, bond).await.expect("insert");

        let found = find_bond_by_hash(&pool, &"c".repeat(64))
            .await
            .expect("query")
            .expect("row present");
        assert_eq!(found.id, created.id);

        let missing = find_bond_by_hash(&pool, &"f".repeat(64))
            .await
            .expect("query");
        assert!(missing.is_none());
    }

    #[tokio::test]
    async fn active_bonds_filter_terminal_states() {
        let pool = setup_pool().await;
        let order_a = Uuid::new_v4();
        let order_b = Uuid::new_v4();
        insert_parent_order(&pool, order_a).await;
        insert_parent_order(&pool, order_b).await;
        let bond_a = create_bond(&pool, dummy_bond(order_a, BondRole::Taker))
            .await
            .unwrap();
        let bond_b = create_bond(&pool, dummy_bond(order_b, BondRole::Taker))
            .await
            .unwrap();

        // Bond B → Released (terminal): must drop out of active set.
        let mut released = bond_b.clone();
        released.state = BondState::Released.to_string();
        update_bond(&pool, released).await.unwrap();

        let active = find_active_bonds(&pool).await.unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].id, bond_a.id);

        let active_a = find_active_bonds_for_order(&pool, order_a).await.unwrap();
        assert_eq!(active_a.len(), 1);
        let active_b = find_active_bonds_for_order(&pool, order_b).await.unwrap();
        assert!(active_b.is_empty());
    }

    #[tokio::test]
    async fn find_by_state_filters() {
        let pool = setup_pool().await;
        let order_a = Uuid::new_v4();
        let order_b = Uuid::new_v4();
        insert_parent_order(&pool, order_a).await;
        insert_parent_order(&pool, order_b).await;
        let bond_a = create_bond(&pool, dummy_bond(order_a, BondRole::Taker))
            .await
            .unwrap();
        let _bond_b = create_bond(&pool, dummy_bond(order_b, BondRole::Maker))
            .await
            .unwrap();

        // Flip one to Locked.
        let mut locked = bond_a.clone();
        locked.state = BondState::Locked.to_string();
        locked.locked_at = Some(42);
        update_bond(&pool, locked).await.unwrap();

        let requested = find_bonds_by_state(&pool, BondState::Requested)
            .await
            .unwrap();
        assert_eq!(requested.len(), 1);
        assert_eq!(requested[0].order_id, order_b);

        let locked = find_bonds_by_state(&pool, BondState::Locked).await.unwrap();
        assert_eq!(locked.len(), 1);
        assert_eq!(locked[0].order_id, order_a);
    }

    #[tokio::test]
    async fn taker_context_columns_roundtrip() {
        // Concurrent-bonds rework adds taker_* columns that stash the
        // deferred take context until the bond locks. Make sure the
        // additive migration is applied and the columns round-trip
        // through insert / fetch.
        let pool = setup_pool().await;
        let order_id = Uuid::new_v4();
        insert_parent_order(&pool, order_id).await;

        let mut bond = dummy_bond(order_id, BondRole::Taker);
        bond.taker_identity = Some("d".repeat(64));
        bond.taker_trade_index = Some(42);
        bond.taker_invoice = Some("lnbc1pTAKER".to_string());
        bond.taker_fiat_amount = Some(123);
        bond.taker_amount = Some(45_678);
        bond.taker_fee = Some(89);
        bond.taker_dev_fee = Some(7);
        let created = create_bond(&pool, bond).await.unwrap();

        let fetched = find_bond_by_order_and_role(&pool, order_id, BondRole::Taker)
            .await
            .unwrap()
            .expect("bond present");
        assert_eq!(fetched.id, created.id);
        assert_eq!(
            fetched.taker_identity.as_deref(),
            Some("d".repeat(64).as_str())
        );
        assert_eq!(fetched.taker_trade_index, Some(42));
        assert_eq!(fetched.taker_invoice.as_deref(), Some("lnbc1pTAKER"));
        assert_eq!(fetched.taker_fiat_amount, Some(123));
        assert_eq!(fetched.taker_amount, Some(45_678));
        assert_eq!(fetched.taker_fee, Some(89));
        assert_eq!(fetched.taker_dev_fee, Some(7));
    }

    #[tokio::test]
    async fn update_preserves_created_at() {
        let pool = setup_pool().await;
        let order_id = Uuid::new_v4();
        insert_parent_order(&pool, order_id).await;

        let mut bond = dummy_bond(order_id, BondRole::Taker);
        bond.created_at = 1;
        let created = create_bond(&pool, bond).await.unwrap();
        assert_eq!(created.created_at, 1);

        let mut updated = created.clone();
        updated.state = BondState::Locked.to_string();
        updated.created_at = 999;
        update_bond(&pool, updated).await.unwrap();

        let fetched = find_bond_by_id(&pool, created.id)
            .await
            .unwrap()
            .expect("bond present");
        assert_eq!(fetched.created_at, 1);
        assert_eq!(fetched.state, BondState::Locked.to_string());
    }

    #[tokio::test]
    async fn find_active_bond_by_taker_scopes_to_pubkey() {
        // Two concurrent prospective takers on the same order each get
        // their own `Requested` bond. The lookup must return exactly
        // the bond belonging to the queried pubkey.
        let pool = setup_pool().await;
        let order_id = Uuid::new_v4();
        insert_parent_order(&pool, order_id).await;

        let mut bond_a = dummy_bond(order_id, BondRole::Taker);
        bond_a.pubkey = "a".repeat(64);
        let created_a = create_bond(&pool, bond_a).await.unwrap();

        let mut bond_b = dummy_bond(order_id, BondRole::Taker);
        bond_b.pubkey = "b".repeat(64);
        let created_b = create_bond(&pool, bond_b).await.unwrap();

        let found_a = find_active_bond_by_taker(&pool, order_id, &"a".repeat(64))
            .await
            .unwrap()
            .expect("bond A present");
        assert_eq!(found_a.id, created_a.id);

        let found_b = find_active_bond_by_taker(&pool, order_id, &"b".repeat(64))
            .await
            .unwrap()
            .expect("bond B present");
        assert_eq!(found_b.id, created_b.id);

        // Unrelated pubkey returns None.
        let missing = find_active_bond_by_taker(&pool, order_id, &"c".repeat(64))
            .await
            .unwrap();
        assert!(missing.is_none());

        // Released (terminal) bonds drop out of the lookup.
        let mut released = created_a.clone();
        released.state = BondState::Released.to_string();
        update_bond(&pool, released).await.unwrap();
        let after_release = find_active_bond_by_taker(&pool, order_id, &"a".repeat(64))
            .await
            .unwrap();
        assert!(after_release.is_none());
    }
}
