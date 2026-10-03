//! Reputation portability (docs/REPUTATION_PORTABILITY.md): importing an
//! attestation another issuer signed, and issuing one.

pub mod import;

use crate::config::types::ReputationImportSettings;
use crate::db::{record_reputation_issuer_key, reputation_issuer_names_for_key};
use mostro_core::error::MostroError;
use nostr_sdk::prelude::Timestamp;
use sqlx::SqlitePool;

/// A configured trust-list key held by a different issuer name, one that
/// imports are recorded under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssuerNameConflict {
    pub key: String,
    pub configured: String,
    pub recorded: Vec<String>,
}

/// Boot check: no configured key is held by a name other than its entry's,
/// whether by its past imports or by the name an earlier boot bound it to.
/// Imports are deduplicated on that name, so renaming an entry, or moving a
/// key to another entry, would let every account it imported import again.
/// A planned rotation adds the new key to the entry first, so binding each
/// key to its name at boot also catches a rename made after the old key has
/// left. A bound name no import is recorded under holds nothing, so the
/// binding follows the entry and the move is logged. Returns the conflicts;
/// the caller refuses to start on any. With none, binds every key.
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
    if conflicts.is_empty() {
        let now = Timestamp::now().as_secs() as i64;
        for issuer in &import.issuers {
            for key in issuer.public_keys() {
                let key = key.to_hex();
                let name = issuer.name.trim();
                if let Some(left) = record_reputation_issuer_key(pool, &key, name, now).await? {
                    tracing::warn!(
                        "[reputation_import] key {key} moved from the entry `{left}` to `{name}`; \
                         no import was recorded under `{left}`"
                    );
                }
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

    /// A planned rotation adds the new key, which boots under the entry's
    /// name before it signs anything; once the old key leaves, renaming the
    /// entry must still be caught although the new key has no imports.
    #[tokio::test]
    async fn renaming_after_rotating_to_a_key_without_imports_is_a_conflict() {
        let pool = pool().await;
        let old = Keys::generate().public_key().to_hex();
        let new = Keys::generate().public_key().to_hex();
        insert_reputation_import(&pool, &row("lnp2pbot", &old, "a"))
            .await
            .unwrap();
        let rotating = settings("lnp2pbot", vec![old, new.clone()]);
        assert!(issuer_name_conflicts(&pool, &rotating)
            .await
            .unwrap()
            .is_empty());

        let conflicts = issuer_name_conflicts(&pool, &settings("bot", vec![new.clone()]))
            .await
            .unwrap();
        assert_eq!(
            conflicts,
            vec![IssuerNameConflict {
                key: new,
                configured: "bot".to_string(),
                recorded: vec!["lnp2pbot".to_string()],
            }]
        );
    }

    async fn binding(pool: &SqlitePool, key: &str) -> Option<(String, i64)> {
        sqlx::query_as(
            "SELECT issuer, first_seen_at FROM reputation_issuer_keys WHERE issuer_key = ?1",
        )
        .bind(key)
        .fetch_optional(pool)
        .await
        .unwrap()
    }

    /// A name no import is recorded under has nothing to deduplicate, so an
    /// entry booted with a mistyped name can be corrected before its first
    /// import: its keys' binding follows the entry. Once an import is
    /// recorded under the corrected name, that name holds the keys.
    #[tokio::test]
    async fn correcting_a_name_no_import_uses_rebinds_its_keys() {
        let pool = pool().await;
        let key = Keys::generate().public_key().to_hex();
        assert!(
            issuer_name_conflicts(&pool, &settings("lnp2pot", vec![key.clone()]))
                .await
                .unwrap()
                .is_empty()
        );

        let corrected = settings("lnp2pbot", vec![key.clone()]);
        assert!(issuer_name_conflicts(&pool, &corrected)
            .await
            .unwrap()
            .is_empty());
        assert_eq!(
            binding(&pool, &key).await.map(|(name, _)| name).as_deref(),
            Some("lnp2pbot")
        );

        insert_reputation_import(&pool, &row("lnp2pbot", &key, "a"))
            .await
            .unwrap();
        let conflicts = issuer_name_conflicts(&pool, &settings("lnp2pot", vec![key]))
            .await
            .unwrap();
        assert_eq!(conflicts.len(), 1);
    }

    /// A key bound to a name an import is recorded under keeps it and its
    /// date, whichever key signed that import.
    #[tokio::test]
    async fn a_key_keeps_a_name_imports_are_recorded_under() {
        let pool = pool().await;
        let signer = Keys::generate().public_key().to_hex();
        let key = Keys::generate().public_key().to_hex();
        insert_reputation_import(&pool, &row("lnp2pbot", &signer, "a"))
            .await
            .unwrap();
        record_reputation_issuer_key(&pool, &key, "lnp2pbot", 100)
            .await
            .unwrap();
        let left = record_reputation_issuer_key(&pool, &key, "renamed", 200)
            .await
            .unwrap();
        assert_eq!(left, None);
        assert_eq!(
            binding(&pool, &key).await,
            Some(("lnp2pbot".to_string(), 100))
        );
    }

    /// A binding no import uses moves, and says which name it left.
    #[tokio::test]
    async fn a_binding_no_import_uses_reports_the_name_it_left() {
        let pool = pool().await;
        let key = Keys::generate().public_key().to_hex();
        let first = record_reputation_issuer_key(&pool, &key, "lnp2pot", 100)
            .await
            .unwrap();
        assert_eq!(first, None);
        let again = record_reputation_issuer_key(&pool, &key, "lnp2pot", 150)
            .await
            .unwrap();
        assert_eq!(again, None);
        let left = record_reputation_issuer_key(&pool, &key, "lnp2pbot", 200)
            .await
            .unwrap();
        assert_eq!(left.as_deref(), Some("lnp2pot"));
        assert_eq!(
            binding(&pool, &key).await,
            Some(("lnp2pbot".to_string(), 200))
        );
    }

    /// A refused boot leaves no trace: the keys of the entries that were fine
    /// are not bound, so fixing the conflict and booting again starts clean.
    #[tokio::test]
    async fn a_refused_boot_binds_none_of_the_other_keys() {
        let pool = pool().await;
        let moved = Keys::generate().public_key().to_hex();
        let fresh = Keys::generate().public_key().to_hex();
        insert_reputation_import(&pool, &row("lnp2pbot", &moved, "a"))
            .await
            .unwrap();

        let entry = |name: &str, key: &String| ReputationIssuer {
            name: name.to_string(),
            keys: vec![key.clone()],
        };
        let two_entries = ReputationImportSettings {
            enabled: true,
            issuers: vec![entry("other-mostro", &moved), entry("new-entry", &fresh)],
            ..Default::default()
        };
        let conflicts = issuer_name_conflicts(&pool, &two_entries).await.unwrap();
        assert_eq!(conflicts.len(), 1);
        assert!(reputation_issuer_names_for_key(&pool, &fresh)
            .await
            .unwrap()
            .is_empty());
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
