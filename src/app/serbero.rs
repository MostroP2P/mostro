//! Serbero, the node's dispute assistant (docs/SOLVER_PERMISSION_LEVELS.md,
//! "Serbero").
//!
//! The operator sets `[mostro].serbero_pubkey` and nothing else: at boot the
//! daemon registers that key as a read-only solver, so the assistant can take
//! disputes and talk to the parties but never settle or cancel. The info event
//! then announces the key, so clients can tell the assistant from a human
//! solver. A key that is already something else stops the boot instead of
//! being converted: changing its permissions silently could strip a human
//! solver's write access after a pasted npub.

use crate::app::admin_add_solver::SOLVER_CATEGORY_READ_ONLY;
use crate::db::add_new_user;
use mostro_core::error::{MostroError, ServiceError};
use mostro_core::user::User;
use nostr_sdk::prelude::PublicKey;
use sqlx::SqlitePool;

/// Outcome of the boot Serbero guard.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SerberoDecision {
    /// The key had no row: registered as a read-only solver, continue.
    Registered,
    /// The key is already a read-only solver: continue.
    ReadOnlySolver,
    /// The key is a solver with another category (write): refuse to start.
    WriteSolver { category: i64 },
    /// The key is a user that is not a solver: refuse to start.
    NotASolver,
    /// The key is the node's own: refuse to start.
    NodeKey,
}

impl SerberoDecision {
    /// Whether the daemon may start with this Serbero.
    pub fn allows_start(&self) -> bool {
        matches!(self, Self::Registered | Self::ReadOnlySolver)
    }
}

/// Make sure the configured Serbero is a read-only solver, registering it
/// when it has no row yet. Never changes an existing row.
pub async fn serbero_guard(
    pool: &SqlitePool,
    serbero: &PublicKey,
    node: &PublicKey,
) -> Result<SerberoDecision, MostroError> {
    if serbero == node {
        return Ok(SerberoDecision::NodeKey);
    }

    let pubkey = serbero.to_hex();
    let existing = sqlx::query_as::<_, User>("SELECT * FROM users WHERE pubkey = ?1")
        .bind(&pubkey)
        .fetch_optional(pool)
        .await
        .map_err(|e| MostroError::MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

    let Some(user) = existing else {
        add_new_user(
            pool,
            User::new(pubkey, 0, 1, 0, SOLVER_CATEGORY_READ_ONLY, 0),
        )
        .await?;
        return Ok(SerberoDecision::Registered);
    };

    Ok(if user.is_solver == 0 {
        SerberoDecision::NotASolver
    } else if user.category == SOLVER_CATEGORY_READ_ONLY {
        SerberoDecision::ReadOnlySolver
    } else {
        SerberoDecision::WriteSolver {
            category: user.category,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::admin_add_solver::SOLVER_CATEGORY_READ_WRITE;
    use crate::db::is_user_present;
    use nostr_sdk::prelude::Keys;

    async fn create_test_pool() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!().run(&pool).await.unwrap();
        pool
    }

    #[tokio::test]
    async fn registers_an_unknown_key_as_a_read_only_solver() {
        let pool = create_test_pool().await;
        let serbero = Keys::generate().public_key();

        let decision = serbero_guard(&pool, &serbero, &Keys::generate().public_key())
            .await
            .unwrap();

        assert_eq!(decision, SerberoDecision::Registered);
        let user = is_user_present(&pool, serbero.to_hex()).await.unwrap();
        assert_eq!(user.is_solver, 1);
        assert_eq!(user.is_admin, 0);
        assert_eq!(user.category, SOLVER_CATEGORY_READ_ONLY);
    }

    #[tokio::test]
    async fn accepts_an_existing_read_only_solver_and_is_idempotent() {
        let pool = create_test_pool().await;
        let serbero = Keys::generate().public_key();
        let node = Keys::generate().public_key();

        serbero_guard(&pool, &serbero, &node).await.unwrap();
        let decision = serbero_guard(&pool, &serbero, &node).await.unwrap();

        assert_eq!(decision, SerberoDecision::ReadOnlySolver);
        assert!(decision.allows_start());
    }

    #[tokio::test]
    async fn refuses_a_write_solver_without_changing_it() {
        let pool = create_test_pool().await;
        let key = Keys::generate().public_key();
        add_new_user(
            &pool,
            User::new(key.to_hex(), 0, 1, 0, SOLVER_CATEGORY_READ_WRITE, 0),
        )
        .await
        .unwrap();

        let decision = serbero_guard(&pool, &key, &Keys::generate().public_key())
            .await
            .unwrap();

        assert_eq!(
            decision,
            SerberoDecision::WriteSolver {
                category: SOLVER_CATEGORY_READ_WRITE
            }
        );
        assert!(!decision.allows_start());
        let user = is_user_present(&pool, key.to_hex()).await.unwrap();
        assert_eq!(user.category, SOLVER_CATEGORY_READ_WRITE);
    }

    #[tokio::test]
    async fn refuses_a_user_that_is_not_a_solver_without_changing_it() {
        let pool = create_test_pool().await;
        let key = Keys::generate().public_key();
        add_new_user(&pool, User::new(key.to_hex(), 0, 0, 0, 0, 3))
            .await
            .unwrap();

        let decision = serbero_guard(&pool, &key, &Keys::generate().public_key())
            .await
            .unwrap();

        assert_eq!(decision, SerberoDecision::NotASolver);
        assert!(!decision.allows_start());
        let user = is_user_present(&pool, key.to_hex()).await.unwrap();
        assert_eq!(user.is_solver, 0);
    }

    #[tokio::test]
    async fn refuses_the_node_key() {
        let pool = create_test_pool().await;
        let node = Keys::generate().public_key();

        let decision = serbero_guard(&pool, &node, &node).await.unwrap();

        assert_eq!(decision, SerberoDecision::NodeKey);
        assert!(!decision.allows_start());
        assert!(is_user_present(&pool, node.to_hex()).await.is_err());
    }
}
