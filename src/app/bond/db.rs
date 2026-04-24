//! Database helpers for the `bonds` table.
//!
//! Phase 0 exposes the CRUD surface later phases will need. Nothing in
//! this module hits LND or the Nostr client — it's purely storage.

use mostro_core::error::{MostroError::MostroInternalErr, ServiceError};
use sqlx::{Pool, Sqlite};
use sqlx_crud::Crud;
use uuid::Uuid;

use super::model::Bond;
use super::types::{BondRole, BondState};

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
    use crate::app::bond::model::Bond;
    use crate::app::bond::types::BondRole;
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

    fn dummy_bond(order_id: Uuid, role: BondRole) -> Bond {
        Bond::new_requested(order_id, "a".repeat(64), role, 1_500)
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
}
