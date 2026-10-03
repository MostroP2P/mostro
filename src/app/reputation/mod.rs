//! Reputation portability (docs/REPUTATION_PORTABILITY.md): importing an
//! attestation another issuer signed, and issuing one.

pub mod import;

use crate::config::types::ReputationImportSettings;
use crate::db::reputation_issuer_names_for_key;
use mostro_core::error::MostroError;
use sqlx::SqlitePool;

/// A configured trust-list key that already has imports recorded under a
/// different issuer name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssuerNameConflict {
    pub key: String,
    pub configured: String,
    pub recorded: Vec<String>,
}

/// Boot check: every configured key's past imports were recorded under the
/// name its entry has now. Imports are deduplicated on that name, so
/// renaming an entry, or moving a key to another entry, would let every
/// account it imported import again. Returns the conflicts; the caller
/// refuses to start on any.
pub async fn issuer_name_conflicts(
    pool: &SqlitePool,
    import: &ReputationImportSettings,
) -> Result<Vec<IssuerNameConflict>, MostroError> {
    let mut conflicts = Vec::new();
    for issuer in &import.issuers {
        for key in issuer.public_keys() {
            let key = key.to_hex();
            let recorded: Vec<String> = reputation_issuer_names_for_key(pool, &key)
                .await?
                .into_iter()
                .filter(|name| name != issuer.name.trim())
                .collect();
            if !recorded.is_empty() {
                conflicts.push(IssuerNameConflict {
                    key,
                    configured: issuer.name.trim().to_string(),
                    recorded,
                });
            }
        }
    }
    Ok(conflicts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::ReputationIssuer;
    use crate::db::{insert_reputation_import, ReputationImportRow};
    use nostr_sdk::prelude::Keys;

    async fn pool() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!().run(&pool).await.unwrap();
        pool
    }

    fn row(issuer: &str, key: &str, subject: &str) -> ReputationImportRow {
        ReputationImportRow {
            attestation_id: format!("{subject:0>64}"),
            issuer: issuer.to_string(),
            issuer_key: key.to_string(),
            subject: subject.to_string(),
            identity_pubkey: Keys::generate().public_key().to_hex(),
            trade_pubkey: None,
            reviews: 5,
            rating_hundredths: 450,
            since: 1_696_204_800,
            imported_at: 1_790_000_000,
        }
    }

    fn settings(name: &str, keys: Vec<String>) -> ReputationImportSettings {
        ReputationImportSettings {
            enabled: true,
            issuers: vec![ReputationIssuer {
                name: name.to_string(),
                keys,
            }],
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn a_key_whose_imports_carry_its_current_name_is_fine() {
        let pool = pool().await;
        let key = Keys::generate().public_key().to_hex();
        insert_reputation_import(&pool, &row("lnp2pbot", &key, "a"))
            .await
            .unwrap();
        let conflicts = issuer_name_conflicts(&pool, &settings("lnp2pbot", vec![key]))
            .await
            .unwrap();
        assert!(conflicts.is_empty());
    }

    #[tokio::test]
    async fn renaming_an_entry_with_imports_is_a_conflict() {
        let pool = pool().await;
        let key = Keys::generate().public_key().to_hex();
        insert_reputation_import(&pool, &row("lnp2pbot", &key, "a"))
            .await
            .unwrap();
        let conflicts = issuer_name_conflicts(&pool, &settings("bot", vec![key.clone()]))
            .await
            .unwrap();
        assert_eq!(
            conflicts,
            vec![IssuerNameConflict {
                key,
                configured: "bot".to_string(),
                recorded: vec!["lnp2pbot".to_string()],
            }]
        );
    }

    #[tokio::test]
    async fn a_second_import_of_the_same_source_or_identity_is_refused() {
        let pool = pool().await;
        let key = Keys::generate().public_key().to_hex();
        let first = row("lnp2pbot", &key, "a");
        insert_reputation_import(&pool, &first).await.unwrap();

        let same_subject = ReputationImportRow {
            attestation_id: "b".repeat(64),
            identity_pubkey: Keys::generate().public_key().to_hex(),
            ..first.clone()
        };
        let same_identity = ReputationImportRow {
            attestation_id: "c".repeat(64),
            subject: "another-account".to_string(),
            ..first.clone()
        };
        for duplicate in [same_subject, same_identity] {
            assert!(matches!(
                insert_reputation_import(&pool, &duplicate).await,
                Err(MostroError::MostroCantDo(
                    mostro_core::error::CantDoReason::ReputationAlreadyImported
                ))
            ));
        }
        // Another issuer for the same identity is a different source.
        let other_issuer = ReputationImportRow {
            attestation_id: "d".repeat(64),
            issuer: "other-mostro".to_string(),
            ..first
        };
        insert_reputation_import(&pool, &other_issuer)
            .await
            .unwrap();
    }
}

#[cfg(test)]
mod schema_tests {
    use crate::db::{add_new_user, is_user_present, update_user_rating};
    use mostro_core::user::User;
    use nostr_sdk::prelude::Keys;
    use sqlx::SqlitePool;

    async fn pool() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!().run(&pool).await.unwrap();
        pool
    }

    /// A row as it stood before the migration — native columns at their
    /// column defaults — gets the backfill: the displayed average times the
    /// reviews, and the date it shows.
    #[tokio::test]
    async fn the_backfill_gives_a_legacy_row_its_displayed_figures() {
        let pool = pool().await;
        let pubkey = Keys::generate().public_key().to_hex();
        sqlx::query(
            "INSERT INTO users (pubkey, total_reviews, total_rating, created_at, \
             native_rating_sum, native_created_at) VALUES (?1, 4, 4.25, 1700000000, 0.0, NULL)",
        )
        .bind(&pubkey)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::raw_sql(include_str!(
            "../../../migrations/20261003120100_reputation_imports.sql"
        ))
        .execute(&pool)
        .await
        .unwrap();
        let user = is_user_present(&pool, pubkey).await.unwrap();
        assert_eq!(user.native_rating_sum, 17.0);
        assert_eq!(user.native_created_at, Some(1_700_000_000));
        assert_eq!(user.native_stats(), (4, 4.25));
    }

    #[tokio::test]
    async fn a_new_user_records_its_native_date() {
        let pool = pool().await;
        let pubkey = Keys::generate().public_key().to_hex();
        add_new_user(&pool, User::new(pubkey.clone(), 0, 0, 0, 0, 1))
            .await
            .unwrap();
        let user = is_user_present(&pool, pubkey).await.unwrap();
        assert_eq!(user.native_created_at, Some(user.created_at));
        assert_eq!(user.native_rating_sum, 0.0);
    }

    #[tokio::test]
    async fn a_rating_received_after_the_migration_moves_the_native_sum() {
        let pool = pool().await;
        let pubkey = Keys::generate().public_key().to_hex();
        add_new_user(&pool, User::new(pubkey.clone(), 0, 0, 0, 0, 1))
            .await
            .unwrap();
        let mut user = is_user_present(&pool, pubkey.clone()).await.unwrap();
        user.update_rating(5);
        update_user_rating(
            &pool,
            pubkey.clone(),
            user.last_rating,
            user.min_rating,
            user.max_rating,
            user.total_reviews,
            user.total_rating,
            user.native_rating_sum,
        )
        .await
        .unwrap();
        let stored = is_user_present(&pool, pubkey).await.unwrap();
        assert_eq!(stored.total_rating, 2.5);
        assert_eq!(stored.native_rating_sum, 5.0);
        assert_eq!(stored.native_stats(), (1, 5.0));
    }
}
