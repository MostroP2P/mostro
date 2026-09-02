//! Key/value accessors for the `daemon_state` table.
//!
//! Operator-controlled state that is flipped at runtime over the admin RPC and
//! must survive a restart — unlike `settings.toml`, which is read once at boot
//! into an immutable `Arc<Settings>`. See
//! `docs/MAINTENANCE_MODE_LN_MIGRATION.md` §3.1.

use mostro_core::error::MostroError::{self, MostroInternalErr};
use mostro_core::error::ServiceError;
use sqlx::SqlitePool;

/// Read one key. `Ok(None)` when the key was never written.
pub async fn get(pool: &SqlitePool, key: &str) -> Result<Option<String>, MostroError> {
    sqlx::query_scalar::<_, String>("SELECT value FROM daemon_state WHERE key = ?1")
        .bind(key)
        .fetch_optional(pool)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))
}

const UPSERT: &str = "INSERT INTO daemon_state (key, value, updated_at) VALUES (?1, ?2, ?3) \
     ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at";

/// Upsert one key with the current unix timestamp.
pub async fn set(pool: &SqlitePool, key: &str, value: &str) -> Result<(), MostroError> {
    sqlx::query(UPSERT)
        .bind(key)
        .bind(value)
        .bind(chrono::Utc::now().timestamp())
        .execute(pool)
        .await
        .map(|_| ())
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))
}

/// Same upsert inside a caller-owned transaction, so several keys can be
/// written atomically.
pub async fn set_in(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    key: &str,
    value: &str,
) -> Result<(), MostroError> {
    sqlx::query(UPSERT)
        .bind(key)
        .bind(value)
        .bind(chrono::Utc::now().timestamp())
        .execute(&mut **tx)
        .await
        .map(|_| ())
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pool() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        pool
    }

    #[tokio::test]
    async fn get_returns_none_for_an_unknown_key() {
        let pool = pool().await;
        assert_eq!(get(&pool, "nope").await.unwrap(), None);
    }

    #[tokio::test]
    async fn set_in_is_atomic_across_keys() {
        let pool = pool().await;
        let mut tx = pool.begin().await.unwrap();
        set_in(&mut tx, "a", "1").await.unwrap();
        set_in(&mut tx, "b", "2").await.unwrap();
        tx.rollback().await.unwrap();
        assert_eq!(get(&pool, "a").await.unwrap(), None, "rolled back");
        assert_eq!(get(&pool, "b").await.unwrap(), None, "rolled back");

        let mut tx = pool.begin().await.unwrap();
        set_in(&mut tx, "a", "1").await.unwrap();
        set_in(&mut tx, "b", "2").await.unwrap();
        tx.commit().await.unwrap();
        assert_eq!(get(&pool, "a").await.unwrap().as_deref(), Some("1"));
        assert_eq!(get(&pool, "b").await.unwrap().as_deref(), Some("2"));
    }

    #[tokio::test]
    async fn set_then_get_round_trips_and_overwrites() {
        let pool = pool().await;
        set(&pool, "k", "1").await.unwrap();
        assert_eq!(get(&pool, "k").await.unwrap().as_deref(), Some("1"));
        set(&pool, "k", "0").await.unwrap();
        assert_eq!(get(&pool, "k").await.unwrap().as_deref(), Some("0"));
    }
}
