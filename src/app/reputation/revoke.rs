//! Revoking the imports made with an issuer key, after it was compromised
//! (docs/REPUTATION_PORTABILITY.md §5.3, "Revoking an issuer key").

use crate::app::rate_user::rating_event_tags;
use crate::db::{
    delete_reputation_import, reputation_imports_by_key, reputation_imports_for_identity,
    update_user_reputation, ReputationImportRow,
};
use crate::util::update_user_rating_event;
use mostro_core::prelude::*;
use nostr_sdk::prelude::*;
use sqlx::SqlitePool;

/// Reverse every import signed by `issuer_key`, or with `imported_after` only
/// those this node recorded at or after that time — the node's own clock,
/// never the attestation's `created_at`, which whoever holds the key
/// chooses. Each import is reversed and its row deleted in one transaction,
/// so its user can import again with an attestation from the new key; the
/// user's rating event is republished under the trade key that imported.
/// Imports signed by the entry's other keys are untouched. Returns the
/// imports reversed.
pub async fn revoke_imports(
    pool: &SqlitePool,
    issuer_key: &PublicKey,
    imported_after: Option<i64>,
    keys: &Keys,
) -> Result<Vec<ReputationImportRow>, MostroError> {
    let rows = reputation_imports_by_key(pool, &issuer_key.to_hex(), imported_after).await?;
    for row in &rows {
        let user = revert(pool, row).await?;
        tracing::warn!(
            "reputation: revoked import {} by {} ({} ratings from `{}`, key {})",
            row.attestation_id,
            row.identity_pubkey,
            row.reviews,
            row.issuer,
            row.issuer_key
        );
        if let Some(trade_pubkey) = &row.trade_pubkey {
            update_user_rating_event(trade_pubkey, rating_event_tags(&user), keys).await?;
        }
    }
    Ok(rows)
}

/// Reverse one import and delete its row, in one transaction.
async fn revert(pool: &SqlitePool, row: &ReputationImportRow) -> Result<User, MostroError> {
    let db_err = |e: sqlx::Error| MostroInternalErr(ServiceError::DbAccessError(e.to_string()));
    let mut tx = pool.begin().await.map_err(db_err)?;
    let mut user = sqlx::query_as::<_, User>("SELECT * FROM users WHERE pubkey = ?1")
        .bind(&row.identity_pubkey)
        .fetch_one(&mut *tx)
        .await
        .map_err(db_err)?;
    delete_reputation_import(&mut *tx, &row.attestation_id).await?;
    let remaining_since = reputation_imports_for_identity(&mut *tx, &row.identity_pubkey)
        .await?
        .iter()
        .map(|other| other.since)
        .min();
    user.revert_reputation_import(&row.figures()?, remaining_since);
    update_user_reputation(&mut *tx, &user).await?;
    tx.commit().await.map_err(db_err)?;
    Ok(user)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::context::test_utils::{test_settings, TestContextBuilder};
    use crate::app::context::AppContext;
    use crate::app::reputation::import::import_reputation_action;
    use crate::config::types::{ReputationImportSettings, ReputationIssuer};
    use crate::db::{add_new_user, is_user_present};
    use mostro_core::reputation::ATTESTATION_LIFETIME_SECS;
    use sqlx::sqlite::SqlitePoolOptions;
    use std::sync::Arc;

    async fn ctx(keys: &[&Keys]) -> AppContext {
        let _ = crate::config::MOSTRO_CONFIG.set(test_settings());
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        let mut settings = test_settings();
        settings.reputation_import = Some(ReputationImportSettings {
            enabled: true,
            issuers: vec![ReputationIssuer {
                name: "lnp2pbot".to_string(),
                keys: keys.iter().map(|k| k.public_key().to_hex()).collect(),
            }],
            ..Default::default()
        });
        TestContextBuilder::new()
            .with_pool(Arc::new(pool))
            .with_settings(settings)
            .build()
    }

    /// Import an attestation signed at `signed_at` for `identity`, from a
    /// fresh trade key, and return that key.
    async fn import(
        ctx: &AppContext,
        issuer: &Keys,
        identity: &PublicKey,
        subject: &str,
        signed_at: Timestamp,
    ) -> PublicKey {
        import_since(ctx, issuer, identity, subject, signed_at, 1_696_204_800).await
    }

    async fn import_since(
        ctx: &AppContext,
        issuer: &Keys,
        identity: &PublicKey,
        subject: &str,
        signed_at: Timestamp,
        since: u64,
    ) -> PublicKey {
        let json = ReputationAttestation::build(
            issuer,
            identity,
            subject,
            100,
            4.5,
            since,
            signed_at,
            ATTESTATION_LIFETIME_SECS,
        )
        .unwrap()
        .as_json();
        let msg = Message::new_order(
            None,
            None,
            None,
            Action::ImportReputation,
            Some(Payload::ReputationAttestation(json)),
        );
        let sender = Keys::generate().public_key();
        let event = UnwrappedMessage {
            message: msg.clone(),
            signature: None,
            sender,
            identity: *identity,
            created_at: Timestamp::now(),
        };
        import_reputation_action(ctx, msg, &event, ctx.keys())
            .await
            .unwrap();
        sender
    }

    async fn user(ctx: &AppContext, identity: &PublicKey) -> User {
        is_user_present(ctx.pool(), identity.to_hex())
            .await
            .unwrap()
    }

    async fn separate_issuers(keys: &[&Keys]) -> AppContext {
        let base = ctx(keys).await;
        let mut settings = base.settings().clone();
        settings.reputation_import.as_mut().unwrap().issuers = keys
            .iter()
            .enumerate()
            .map(|(i, key)| ReputationIssuer {
                name: format!("issuer-{i}"),
                keys: vec![key.public_key().to_hex()],
            })
            .collect();
        TestContextBuilder::new()
            .with_pool(base.pool_arc())
            .with_settings(settings)
            .with_keys(base.keys().clone())
            .build()
    }

    fn tag(event: &Event, name: &str) -> Option<String> {
        event.tags.iter().find_map(|tag| {
            let values = tag.as_slice();
            (values.first().map(String::as_str) == Some(name))
                .then(|| values.get(1).cloned())
                .flatten()
        })
    }

    async fn rating_events(ctx: &AppContext, trade_key: PublicKey) -> Vec<Event> {
        crate::config::MESSAGE_QUEUES
            .queue_order_rate
            .read()
            .await
            .iter()
            .filter(|event| {
                event.pubkey == ctx.keys().public_key()
                    && tag(event, "d") == Some(trade_key.to_hex())
            })
            .cloned()
            .collect()
    }

    #[tokio::test]
    async fn a_row_reversed_by_another_call_is_not_reversed_twice() {
        let issuer = Keys::generate();
        let ctx = ctx(&[&issuer]).await;
        let identity = Keys::generate().public_key();
        let mut native = User::new(identity.to_hex(), 0, 0, 0, 0, 1);
        native.update_rating(5);
        native.update_rating(3);
        add_new_user(ctx.pool(), native).await.unwrap();
        import(&ctx, &issuer, &identity, "stale-snapshot", Timestamp::now()).await;
        let cached = reputation_imports_by_key(ctx.pool(), &issuer.public_key().to_hex(), None)
            .await
            .unwrap()
            .remove(0);
        revoke_imports(ctx.pool(), &issuer.public_key(), None, ctx.keys())
            .await
            .unwrap();
        let after_first = user(&ctx, &identity).await;
        assert_eq!(after_first.total_reviews, 2);

        // Another RPC selected this receipt before the first one committed.
        revert(ctx.pool(), &cached).await.unwrap();
        assert_eq!(user(&ctx, &identity).await, after_first);
    }

    #[tokio::test]
    async fn overlapping_revocations_reverse_each_import_once() {
        use sqlx::sqlite::SqliteConnectOptions;

        let path = std::env::temp_dir().join(format!("mostro-revoke-{}.db", uuid::Uuid::new_v4()));
        let pool = SqlitePoolOptions::new()
            .min_connections(2)
            .max_connections(4)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(&path)
                    .create_if_missing(true),
            )
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        let issuer = Keys::generate();
        let base = ctx(&[&issuer]).await;
        let ctx = TestContextBuilder::new()
            .with_pool(Arc::new(pool))
            .with_settings(base.settings().clone())
            .with_keys(base.keys().clone())
            .build();
        for round in 0..4 {
            let mut identities = Vec::new();
            for n in 0..16 {
                let identity = Keys::generate().public_key();
                let mut native = User::new(identity.to_hex(), 0, 0, 0, 0, 1);
                native.update_rating(5);
                native.update_rating(3);
                add_new_user(ctx.pool(), native).await.unwrap();
                import(
                    &ctx,
                    &issuer,
                    &identity,
                    &format!("{round}-{n}"),
                    Timestamp::now(),
                )
                .await;
                identities.push(identity);
            }
            let key = issuer.public_key();
            let (a, b) = tokio::join!(
                revoke_imports(ctx.pool(), &key, None, ctx.keys()),
                revoke_imports(ctx.pool(), &key, None, ctx.keys())
            );
            assert_eq!(a.unwrap().len() + b.unwrap().len(), identities.len());
            for identity in identities {
                let user = user(&ctx, &identity).await;
                assert_eq!(user.total_reviews, 2);
                assert!((user.total_rating - 2.75).abs() < 1e-9);
                assert_eq!(user.native_stats(), (2, 4.0));
                assert_eq!(user.seeded_reviews, 0);
                assert!(user.seeded_rating_sum.abs() < 1e-9);
            }
        }
        ctx.pool().close().await;
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn revoking_restores_the_earliest_remaining_import_date() {
        let (a, b, c) = (Keys::generate(), Keys::generate(), Keys::generate());
        let ctx = separate_issuers(&[&a, &b, &c]).await;
        let identity = Keys::generate().public_key();
        for (key, since) in [
            (&a, 1_609_459_200),
            (&b, 1_640_995_200),
            (&c, 1_672_531_200),
        ] {
            import_since(&ctx, key, &identity, "account", Timestamp::now(), since).await;
        }
        revoke_imports(ctx.pool(), &a.public_key(), None, ctx.keys())
            .await
            .unwrap();
        let after = user(&ctx, &identity).await;
        assert_eq!(after.total_reviews, 200);
        assert_eq!(after.created_at, 1_640_995_200);
        revoke_imports(ctx.pool(), &b.public_key(), None, ctx.keys())
            .await
            .unwrap();
        assert_eq!(user(&ctx, &identity).await.created_at, 1_672_531_200);
    }

    #[tokio::test]
    async fn revoking_updates_the_trade_keys_of_remaining_imports() {
        let (a, b) = (Keys::generate(), Keys::generate());
        let ctx = separate_issuers(&[&a, &b]).await;
        let identity = Keys::generate().public_key();
        let first = import(&ctx, &a, &identity, "first", Timestamp::now()).await;
        let second = import(&ctx, &b, &identity, "second", Timestamp::now()).await;
        assert_eq!(
            tag(
                rating_events(&ctx, second).await.last().unwrap(),
                "total_reviews"
            )
            .as_deref(),
            Some("200")
        );

        revoke_imports(ctx.pool(), &a.public_key(), None, ctx.keys())
            .await
            .unwrap();
        for trade_key in [first, second] {
            let events = rating_events(&ctx, trade_key).await;
            let latest = events.last().unwrap();
            latest.verify().unwrap();
            assert_eq!(tag(latest, "total_reviews").as_deref(), Some("100"));
            assert_eq!(tag(latest, "total_rating").as_deref(), Some("4.5"));
            assert_eq!(latest.kind.as_u16(), NOSTR_RATING_EVENT_KIND);
            assert!(events.len() >= 2);
            assert!(latest.created_at > events[events.len() - 2].created_at);
        }
    }

    #[tokio::test]
    async fn revoking_supersedes_the_import_event_under_the_same_node_key() {
        let issuer = Keys::generate();
        let ctx = ctx(&[&issuer]).await;
        let identity = Keys::generate().public_key();
        let trade = import(&ctx, &issuer, &identity, "same-second", Timestamp::now()).await;
        revoke_imports(ctx.pool(), &issuer.public_key(), None, ctx.keys())
            .await
            .unwrap();
        let events = rating_events(&ctx, trade).await;
        assert_eq!(events.len(), 2);
        assert_eq!(tag(&events[1], "total_reviews").as_deref(), Some("0"));
        assert!(events[1].created_at > events[0].created_at);
    }

    #[tokio::test]
    async fn revoking_queues_only_the_affected_makers_open_orders() {
        let _guard = crate::util::ORDERBOOK_QUEUE_TEST_LOCK.lock().await;
        let issuer = Keys::generate();
        let ctx = ctx(&[&issuer]).await;
        let identity = Keys::generate().public_key();
        let other = Keys::generate().public_key();
        import(&ctx, &issuer, &identity, "maker", Timestamp::now()).await;
        let mut orders = Vec::new();
        for (kind, status, buyer, seller, expected) in [
            ("sell", "pending", other, identity, true),
            ("buy", "pending", identity, other, true),
            ("sell", "waiting-taker-bond", other, identity, true),
            ("buy", "waiting-taker-bond", identity, other, true),
            ("sell", "active", other, identity, false),
            ("sell", "pending", identity, other, false),
            ("buy", "pending", other, identity, false),
            ("sell", "waiting-maker-bond", other, identity, false),
        ] {
            let id = uuid::Uuid::new_v4();
            sqlx::query(
                "INSERT INTO orders (id, kind, event_id, status, premium, payment_method, \
                 amount, fiat_code, fiat_amount, created_at, expires_at, master_buyer_pubkey, master_seller_pubkey) \
                 VALUES (?1, ?2, 'event', ?3, 0, 'lightning', 1000, 'USD', 1, 1700000000, 1900000000, ?4, ?5)"
            )
            .bind(id).bind(kind).bind(status).bind(buyer.to_hex()).bind(seller.to_hex())
            .execute(ctx.pool()).await.unwrap();
            orders.push((id, expected));
        }
        revoke_imports(ctx.pool(), &issuer.public_key(), None, ctx.keys())
            .await
            .unwrap();
        for (id, expected) in orders {
            assert_eq!(
                crate::util::is_orderbook_publish_queued(id),
                expected,
                "order {id}"
            );
            crate::util::clear_orderbook_publish_failure_up_to(id, u64::MAX);
        }
    }

    #[tokio::test]
    async fn revoking_a_key_restores_its_users_and_leaves_the_other_keys_imports() {
        let (compromised, rotated) = (Keys::generate(), Keys::generate());
        let ctx = ctx(&[&compromised, &rotated]).await;
        let (a, b) = (Keys::generate().public_key(), Keys::generate().public_key());
        let mut native = User::new(a.to_hex(), 0, 0, 0, 0, 1);
        native.update_rating(5);
        native.update_rating(3);
        add_new_user(ctx.pool(), native).await.unwrap();
        let before = user(&ctx, &a).await;

        import(&ctx, &compromised, &a, "acct-a", Timestamp::now()).await;
        import(&ctx, &rotated, &b, "acct-b", Timestamp::now()).await;
        let revoked = revoke_imports(
            ctx.pool(),
            &compromised.public_key(),
            None,
            &Keys::generate(),
        )
        .await
        .unwrap();
        assert_eq!(revoked.len(), 1);

        let after = user(&ctx, &a).await;
        assert_eq!(after.total_reviews, before.total_reviews);
        assert!((after.total_rating - before.total_rating).abs() < 1e-9);
        assert_eq!(after.created_at, before.created_at);
        assert_eq!((after.seeded_reviews, after.seeded_rating_sum), (0, 0.0));
        assert_eq!(
            (after.min_rating, after.max_rating, after.last_rating),
            (before.min_rating, before.max_rating, before.last_rating)
        );
        assert!(reputation_imports_for_identity(ctx.pool(), &a.to_hex())
            .await
            .unwrap()
            .is_empty());
        // The rotated key's import stands.
        assert_eq!(user(&ctx, &b).await.total_reviews, 100);
    }

    #[tokio::test]
    async fn a_backdated_attestation_imported_after_the_cutoff_is_revoked() {
        let compromised = Keys::generate();
        let ctx = ctx(&[&compromised]).await;
        let identity = Keys::generate().public_key();
        // Signed "three days ago" but imported now, after the cutoff.
        let backdated = Timestamp::from(Timestamp::now().as_secs() - 3 * 86_400);
        import(&ctx, &compromised, &identity, "acct-1", backdated).await;
        let cutoff = Timestamp::now().as_secs() as i64 - 60;
        let revoked = revoke_imports(
            ctx.pool(),
            &compromised.public_key(),
            Some(cutoff),
            &Keys::generate(),
        )
        .await
        .unwrap();
        assert_eq!(revoked.len(), 1);
        assert_eq!(user(&ctx, &identity).await.total_reviews, 0);

        // Imports recorded before the cutoff stay.
        let other = Keys::generate().public_key();
        import(&ctx, &compromised, &other, "acct-2", Timestamp::now()).await;
        let later = Timestamp::now().as_secs() as i64 + 60;
        let none = revoke_imports(
            ctx.pool(),
            &compromised.public_key(),
            Some(later),
            &Keys::generate(),
        )
        .await
        .unwrap();
        assert!(none.is_empty());
    }

    #[tokio::test]
    async fn a_native_rating_received_after_the_import_survives_its_revocation() {
        let compromised = Keys::generate();
        let ctx = ctx(&[&compromised]).await;
        let identity = Keys::generate().public_key();
        import(&ctx, &compromised, &identity, "acct-1", Timestamp::now()).await;
        let mut merged = user(&ctx, &identity).await;
        merged.update_rating(1);
        crate::db::update_user_rating(
            ctx.pool(),
            identity.to_hex(),
            merged.last_rating,
            merged.min_rating,
            merged.max_rating,
            merged.total_reviews,
            merged.total_rating,
            merged.native_rating_sum,
        )
        .await
        .unwrap();

        revoke_imports(
            ctx.pool(),
            &compromised.public_key(),
            None,
            &Keys::generate(),
        )
        .await
        .unwrap();
        let after = user(&ctx, &identity).await;
        assert_eq!(after.total_reviews, 1);
        assert!((after.total_rating - 1.0).abs() < 1e-9);
        assert_eq!(after.native_stats(), (1, 1.0));
    }

    #[tokio::test]
    async fn a_revoked_user_can_import_again_with_the_new_key() {
        let (compromised, rotated) = (Keys::generate(), Keys::generate());
        let ctx = ctx(&[&compromised, &rotated]).await;
        let identity = Keys::generate().public_key();
        import(&ctx, &compromised, &identity, "acct-1", Timestamp::now()).await;
        revoke_imports(
            ctx.pool(),
            &compromised.public_key(),
            None,
            &Keys::generate(),
        )
        .await
        .unwrap();
        import(&ctx, &rotated, &identity, "acct-1", Timestamp::now()).await;
        assert_eq!(user(&ctx, &identity).await.total_reviews, 100);
    }

    #[tokio::test]
    async fn revoking_republishes_the_rating_event_under_the_importing_key() {
        let compromised = Keys::generate();
        let ctx = ctx(&[&compromised]).await;
        let identity = Keys::generate().public_key();
        let trade_key = import(&ctx, &compromised, &identity, "acct-1", Timestamp::now()).await;
        let node = Keys::generate();
        revoke_imports(ctx.pool(), &compromised.public_key(), None, &node)
            .await
            .unwrap();
        let tag = |ev: &Event, name: &str| {
            ev.tags.iter().find_map(|t| {
                let v = t.clone().to_vec();
                (v.first().map(String::as_str) == Some(name)).then(|| v[1].clone())
            })
        };
        let republished = crate::config::MESSAGE_QUEUES
            .queue_order_rate
            .read()
            .await
            .iter()
            .any(|ev| {
                ev.pubkey == node.public_key()
                    && tag(ev, "d") == Some(trade_key.to_hex())
                    && tag(ev, "total_reviews") == Some("0".to_string())
            });
        assert!(republished);
    }
}
