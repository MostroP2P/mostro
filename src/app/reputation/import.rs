//! `import-reputation`: a user imports an attestation another issuer signed
//! (docs/REPUTATION_PORTABILITY.md §5.3; MostroP2P/protocol
//! reputation_attestation.md, "Redemption").

use crate::app::context::AppContext;
use crate::app::rate_user::rating_event_tags;
use crate::config::types::ReputationImportSettings;
use crate::db::{insert_reputation_import, update_user_reputation, ReputationImportRow};
use crate::util::{enqueue_order_msg, update_user_rating_event};
use mostro_core::prelude::*;
use mostro_core::reputation::{AttestationError, ReputationImport};
use nostr_sdk::prelude::*;

/// The import settings, only when import is enabled on this node.
pub fn import_settings(ctx: &AppContext) -> Option<&ReputationImportSettings> {
    ctx.settings()
        .reputation_import
        .as_ref()
        .filter(|import| import.enabled)
}

/// Run the redemption checks on an `import-reputation` request, record the
/// import and merge it into the identity's reputation in one transaction,
/// then confirm with `reputation-imported` and queue a rating event for the
/// importing trade key. Exact retries also queue the current aggregate.
/// Every refusal is a `cant-do` whose reason says which check failed.
pub async fn import_reputation_action(
    ctx: &AppContext,
    msg: Message,
    event: &UnwrappedMessage,
    my_keys: &Keys,
) -> Result<(), MostroError> {
    let import = import_settings(ctx).ok_or(MostroCantDo(CantDoReason::InvalidAction))?;
    let kind = msg.get_inner_message_kind();
    let Some(Payload::ReputationAttestation(json)) = kind.get_payload() else {
        return Err(MostroCantDo(CantDoReason::InvalidPayload));
    };
    // Full privacy: the "identity" is the trade key itself, with no proof.
    if event.identity == event.sender {
        return Err(MostroCantDo(CantDoReason::ReputationIdentityRequired));
    }

    // Steps 1, 2 and 4: the event itself and the clock.
    let signed = Event::from_json(json)
        .map_err(|_| MostroCantDo(CantDoReason::InvalidReputationAttestation))?;
    let (attestation, expired) = match ReputationAttestation::parse(
        &signed,
        Timestamp::now(),
        import.max_lifetime_seconds,
    ) {
        Ok(attestation) => (attestation, false),
        Err(AttestationError::Expired) => {
            // Recheck the signed event at issuance, including the lifetime
            // cap. This permits only recovery of an existing receipt below;
            // it must never authorize a new import of an expired event.
            let attestation = ReputationAttestation::parse(
                &signed,
                signed.created_at,
                import.max_lifetime_seconds,
            )
            .map_err(|e| MostroCantDo(e.cant_do_reason()))?;
            (attestation, true)
        }
        Err(e) => return Err(MostroCantDo(e.cant_do_reason())),
    };
    // Step 3: a trusted key, which from here on stands for its entry's name.
    let issuer = import
        .issuer_for(&attestation.issuer)
        .ok_or(MostroCantDo(CantDoReason::UntrustedReputationIssuer))?;
    // Step 5: addressed to the identity the transport proved.
    if attestation.destination != event.identity {
        return Err(MostroCantDo(CantDoReason::ReputationIdentityMismatch));
    }

    let replayed = if expired {
        // A restart can lose the reply until after expiration. Confirm only
        // this exact committed import; a reversal removes its receipt.
        if !is_recorded(ctx, &attestation, event).await? {
            return Err(MostroCantDo(CantDoReason::ExpiredReputationAttestation));
        }
        true
    } else {
        match record_import(ctx, &issuer.name, &attestation, event).await {
            Ok(_) => false,
            // A valid retry, including one racing the original request.
            Err(MostroCantDo(CantDoReason::ReputationAlreadyImported))
                if is_recorded(ctx, &attestation, event).await? =>
            {
                true
            }
            Err(e) => return Err(e),
        }
    };
    // Name the trade key, as the reply's log line does: the identity key
    // would tie a long-lived identity to its import in the logs.
    if replayed {
        tracing::info!(
            "reputation: trade key {} resent attestation {}, already imported",
            event.sender,
            attestation.id
        );
    } else {
        tracing::info!(
            "reputation: trade key {} imported {} ratings from `{}` (attestation {})",
            event.sender,
            attestation.reviews,
            issuer.name,
            attestation.id
        );
    }
    enqueue_order_msg(
        kind.request_id,
        None,
        Action::ReputationImported,
        None,
        event.sender,
        None,
    )
    .await;
    // Read the current aggregate on retries too: the original queued event
    // may have been lost on restart, and later ratings/imports must not be
    // replaced by the figures of this attestation alone.
    let user = crate::db::is_user_present(ctx.pool(), event.identity.to_hex()).await?;
    update_user_rating_event(&event.sender.to_hex(), rating_event_tags(&user), my_keys).await?;
    Ok(())
}

/// Steps 6 to 8: record the import and merge it into the identity's user
/// row, creating the row for an identity that never traded here, all in one
/// transaction. The unique indexes refuse a second import of the same
/// source, or of a second account from the issuer, even under concurrency.
async fn record_import(
    ctx: &AppContext,
    issuer_name: &str,
    attestation: &ReputationAttestation,
    event: &UnwrappedMessage,
) -> Result<User, MostroError> {
    let db_err = |e: sqlx::Error| MostroInternalErr(ServiceError::DbAccessError(e.to_string()));
    let identity_hex = event.identity.to_hex();
    let now = Timestamp::now().as_secs() as i64;
    let mut tx = ctx.pool().begin().await.map_err(db_err)?;

    sqlx::query(
        "INSERT OR IGNORE INTO users (pubkey, created_at, native_created_at) VALUES (?1, ?2, ?2)",
    )
    .bind(&identity_hex)
    .bind(now)
    .execute(&mut *tx)
    .await
    .map_err(db_err)?;
    let mut user = sqlx::query_as::<_, User>("SELECT * FROM users WHERE pubkey = ?1")
        .bind(&identity_hex)
        .fetch_one(&mut *tx)
        .await
        .map_err(db_err)?;

    insert_reputation_import(
        &mut *tx,
        &ReputationImportRow {
            attestation_id: attestation.id.to_hex(),
            issuer: issuer_name.to_string(),
            issuer_key: attestation.issuer.to_hex(),
            subject: attestation.subject.clone(),
            identity_pubkey: identity_hex,
            trade_pubkey: Some(event.sender.to_hex()),
            reviews: i64::from(attestation.reviews),
            rating_hundredths: i64::from(attestation.rating_hundredths),
            since: attestation.since as i64,
            imported_at: now,
        },
    )
    .await?;
    user.apply_reputation_import(&ReputationImport::from(attestation));
    update_user_reputation(&mut *tx, &user).await?;
    tx.commit().await.map_err(db_err)?;
    Ok(user)
}

/// Whether this attestation is already imported for the proved identity.
/// Its id covers every tag, so a match is the same signed figures for the
/// same destination, and a reversed import no longer has a row.
async fn is_recorded(
    ctx: &AppContext,
    attestation: &ReputationAttestation,
    event: &UnwrappedMessage,
) -> Result<bool, MostroError> {
    sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM reputation_imports \
         WHERE attestation_id = ?1 AND identity_pubkey = ?2)",
    )
    .bind(attestation.id.to_hex())
    .bind(event.identity.to_hex())
    .fetch_one(ctx.pool())
    .await
    .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::context::test_utils::{test_settings, TestContextBuilder};
    use crate::config::types::ReputationIssuer;
    use crate::db::{add_new_user, is_user_present, reputation_imports_for_identity};
    use mostro_core::reputation::ATTESTATION_LIFETIME_SECS;
    use sqlx::sqlite::SqlitePoolOptions;
    use std::sync::Arc;

    const SINCE: u64 = 1_696_204_800;

    struct Fixture {
        ctx: AppContext,
        issuer: Keys,
    }

    async fn fixture_with(issuers: Vec<(&str, Vec<String>)>, issuer: Keys) -> Fixture {
        let _ = crate::config::MOSTRO_CONFIG.set(test_settings());
        // One connection: an in-memory SQLite database is per connection.
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        let mut settings = test_settings();
        settings.reputation_import = Some(ReputationImportSettings {
            enabled: true,
            issuers: issuers
                .into_iter()
                .map(|(name, keys)| ReputationIssuer {
                    name: name.to_string(),
                    keys,
                })
                .collect(),
            ..Default::default()
        });
        let ctx = TestContextBuilder::new()
            .with_pool(Arc::new(pool))
            .with_settings(settings)
            .build();
        Fixture { ctx, issuer }
    }

    async fn fixture() -> Fixture {
        let issuer = Keys::generate();
        let key = issuer.public_key().to_hex();
        fixture_with(vec![("lnp2pbot", vec![key])], issuer).await
    }

    fn attestation(issuer: &Keys, destination: &PublicKey, subject: &str) -> String {
        attestation_at(issuer, destination, subject, Timestamp::now())
    }

    /// An event id hashes `created_at` to the second, so two attestations of
    /// one account signed in the same second are the same attestation.
    fn attestation_at(
        issuer: &Keys,
        destination: &PublicKey,
        subject: &str,
        created_at: Timestamp,
    ) -> String {
        ReputationAttestation::build(
            issuer,
            destination,
            subject,
            214,
            4.87,
            SINCE,
            created_at,
            ATTESTATION_LIFETIME_SECS,
        )
        .unwrap()
        .as_json()
    }

    /// The replies queued for `sender`.
    async fn replies_to(sender: PublicKey) -> Vec<Message> {
        crate::config::MESSAGE_QUEUES
            .queue_order_msg
            .read()
            .await
            .iter()
            .filter(|(_, pk)| *pk == sender)
            .map(|(m, _)| m.clone())
            .collect()
    }

    /// An import request from `identity`'s proof, sent by a fresh trade key.
    fn request(identity: &PublicKey, json: &str) -> (Message, UnwrappedMessage) {
        let msg = Message::new_order(
            None,
            Some(7),
            None,
            Action::ImportReputation,
            Some(Payload::ReputationAttestation(json.to_string())),
        );
        let event = UnwrappedMessage {
            message: msg.clone(),
            signature: None,
            sender: Keys::generate().public_key(),
            identity: *identity,
            created_at: Timestamp::now(),
        };
        (msg, event)
    }

    async fn import(f: &Fixture, identity: &PublicKey, json: &str) -> Result<(), MostroError> {
        let (msg, event) = request(identity, json);
        import_reputation_action(&f.ctx, msg, &event, f.ctx.keys()).await
    }

    fn refused(result: Result<(), MostroError>) -> CantDoReason {
        match result {
            Err(MostroCantDo(reason)) => reason,
            other => panic!("expected a cant-do, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_attestation_merges_into_the_identitys_reputation_and_is_confirmed() {
        let f = fixture().await;
        let identity = Keys::generate().public_key();
        let mut user = User::new(identity.to_hex(), 0, 0, 0, 0, 1);
        user.update_rating(4);
        user.update_rating(5);
        add_new_user(f.ctx.pool(), user).await.unwrap();
        let before = is_user_present(f.ctx.pool(), identity.to_hex())
            .await
            .unwrap();

        let (msg, event) = request(&identity, &attestation(&f.issuer, &identity, "acct-1"));
        import_reputation_action(&f.ctx, msg, &event, f.ctx.keys())
            .await
            .unwrap();

        let after = is_user_present(f.ctx.pool(), identity.to_hex())
            .await
            .unwrap();
        assert_eq!(after.total_reviews, before.total_reviews + 214);
        assert_eq!(after.seeded_reviews, 214);
        assert_eq!(after.created_at, SINCE as i64);
        assert_eq!(after.native_created_at, before.native_created_at);
        assert_eq!(after.native_stats().0, 2);
        let rows = reputation_imports_for_identity(f.ctx.pool(), &identity.to_hex())
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            (rows[0].issuer.as_str(), rows[0].subject.as_str()),
            ("lnp2pbot", "acct-1")
        );
        assert_eq!(rows[0].issuer_key, f.issuer.public_key().to_hex());
        assert_eq!(rows[0].trade_pubkey, Some(event.sender.to_hex()));

        let replies = replies_to(event.sender).await;
        assert_eq!(replies.len(), 1);
        let reply = replies[0].get_inner_message_kind();
        assert_eq!(reply.action, Action::ReputationImported);
        assert_eq!(reply.request_id, Some(7));
        assert!(reply.payload.is_none());
    }

    /// The row an import leaves is what a revocation later selects and
    /// reverses with, so its figures and its clock are pinned here.
    #[tokio::test]
    async fn the_import_row_keeps_what_a_reversal_needs() {
        let f = fixture().await;
        let identity = Keys::generate().public_key();
        let mut user = User::new(identity.to_hex(), 0, 0, 0, 0, 1);
        user.update_rating(4);
        user.update_rating(5);
        add_new_user(f.ctx.pool(), user).await.unwrap();
        let before = is_user_present(f.ctx.pool(), identity.to_hex())
            .await
            .unwrap();

        let (msg, event) = request(&identity, &attestation(&f.issuer, &identity, "acct-1"));
        let at = Timestamp::now().as_secs() as i64;
        import_reputation_action(&f.ctx, msg, &event, f.ctx.keys())
            .await
            .unwrap();

        let rows = reputation_imports_for_identity(f.ctx.pool(), &identity.to_hex())
            .await
            .unwrap();
        let row = &rows[0];
        assert_eq!(
            (row.reviews, row.rating_hundredths, row.since),
            (214, 487, SINCE as i64)
        );
        assert!(
            (row.imported_at - at).abs() <= 5,
            "imported_at is the node's own clock"
        );

        // The stored figures undo the merge exactly.
        let mut after = is_user_present(f.ctx.pool(), identity.to_hex())
            .await
            .unwrap();
        after.revert_reputation_import(&row.figures().unwrap(), None);
        assert_eq!(after.total_reviews, before.total_reviews);
        assert!((after.total_rating - before.total_rating).abs() < 1e-9);
        assert_eq!(after.created_at, before.created_at);
        assert_eq!(after.seeded_reviews, 0);
    }

    /// The row an import creates for a new identity dates it from the import,
    /// which is the date a reversal restores.
    #[tokio::test]
    async fn a_new_identitys_native_date_is_when_its_row_was_made() {
        let f = fixture().await;
        let identity = Keys::generate().public_key();
        let at = Timestamp::now().as_secs() as i64;
        import(&f, &identity, &attestation(&f.issuer, &identity, "acct-1"))
            .await
            .unwrap();
        let user = is_user_present(f.ctx.pool(), identity.to_hex())
            .await
            .unwrap();
        assert_eq!(user.created_at, SINCE as i64);
        let native = user.native_created_at.expect("the row has a native date");
        assert!((native - at).abs() <= 5);
    }

    #[tokio::test]
    async fn an_identity_that_never_traded_here_gets_a_row() {
        let f = fixture().await;
        let identity = Keys::generate().public_key();
        import(&f, &identity, &attestation(&f.issuer, &identity, "acct-1"))
            .await
            .unwrap();
        let user = is_user_present(f.ctx.pool(), identity.to_hex())
            .await
            .unwrap();
        assert_eq!(user.total_reviews, 214);
        assert_eq!(user.total_rating, 4.87);
        assert_eq!(
            (user.min_rating, user.max_rating, user.last_rating),
            (5, 5, 5)
        );
        assert_eq!(user.native_stats(), (0, 0.0));
    }

    #[tokio::test]
    async fn a_second_import_of_the_same_source_is_refused() {
        let f = fixture().await;
        let (a, b) = (Keys::generate().public_key(), Keys::generate().public_key());
        import(&f, &a, &attestation(&f.issuer, &a, "acct-1"))
            .await
            .unwrap();
        // The same identity again with another attestation of the account,
        // and the same account for another identity.
        let earlier = Timestamp::from(Timestamp::now().as_secs() - 60);
        assert_eq!(
            refused(import(&f, &a, &attestation_at(&f.issuer, &a, "acct-1", earlier)).await),
            CantDoReason::ReputationAlreadyImported
        );
        assert_eq!(
            refused(import(&f, &b, &attestation(&f.issuer, &b, "acct-1")).await),
            CantDoReason::ReputationAlreadyImported
        );
        // A second account from the same issuer for the same identity.
        assert_eq!(
            refused(import(&f, &a, &attestation(&f.issuer, &a, "acct-2")).await),
            CantDoReason::ReputationAlreadyImported
        );
        let user = is_user_present(f.ctx.pool(), a.to_hex()).await.unwrap();
        assert_eq!(user.total_reviews, 214, "only the first import merged");
    }

    /// The reply queue lives in memory, so a confirmation can be lost after
    /// the import committed. Sending the same attestation again must confirm
    /// it again, not refuse it as a second import, and must merge nothing.
    #[tokio::test]
    async fn a_resent_attestation_is_confirmed_again_and_merges_once() {
        let f = fixture().await;
        let identity = Keys::generate().public_key();
        let json = attestation(&f.issuer, &identity, "acct-1");
        let (msg, first) = request(&identity, &json);
        import_reputation_action(&f.ctx, msg, &first, f.ctx.keys())
            .await
            .unwrap();

        // The retry may come from another trade key.
        let (msg, retry) = request(&identity, &json);
        import_reputation_action(&f.ctx, msg, &retry, f.ctx.keys())
            .await
            .unwrap();

        let replies = replies_to(retry.sender).await;
        assert_eq!(replies.len(), 1);
        let reply = replies[0].get_inner_message_kind();
        assert_eq!(reply.action, Action::ReputationImported);
        assert_eq!(reply.request_id, Some(7));
        let user = is_user_present(f.ctx.pool(), identity.to_hex())
            .await
            .unwrap();
        assert_eq!(user.total_reviews, 214, "the retry merged nothing");
        let rows = reputation_imports_for_identity(f.ctx.pool(), &identity.to_hex())
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].trade_pubkey, Some(first.sender.to_hex()));
    }

    /// Seed an import committed while the attestation was valid, then retry
    /// it after expiration without sleeping or changing the system clock.
    async fn recorded_expired_attestation(f: &Fixture, identity: &PublicKey) -> String {
        let created_at =
            Timestamp::from(Timestamp::now().as_secs() - 2 * ATTESTATION_LIFETIME_SECS);
        let json = attestation_at(&f.issuer, identity, "acct-1", created_at);
        let (attestation, _) =
            ReputationAttestation::parse_json(&json, created_at, ATTESTATION_LIFETIME_SECS)
                .unwrap();
        let (_, event) = request(identity, &json);
        record_import(&f.ctx, "lnp2pbot", &attestation, &event)
            .await
            .unwrap();
        json
    }

    #[tokio::test]
    async fn an_expired_recorded_attestation_is_confirmed_without_writes() {
        let f = fixture().await;
        let identity = Keys::generate().public_key();
        let json = recorded_expired_attestation(&f, &identity).await;
        let mut before = is_user_present(f.ctx.pool(), identity.to_hex())
            .await
            .unwrap();
        // A native rating arrived after the import. A replay must publish
        // the current aggregate, not just the old attestation's figures.
        before.update_rating(2);
        let mut tx = f.ctx.pool().begin().await.unwrap();
        crate::db::update_user_rating(
            &mut *tx,
            before.pubkey.clone(),
            before.last_rating,
            before.min_rating,
            before.max_rating,
            before.total_reviews,
            before.total_rating,
            before.native_rating_sum,
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        let rows = reputation_imports_for_identity(f.ctx.pool(), &identity.to_hex())
            .await
            .unwrap();
        let (msg, retry) = request(&identity, &json);
        import_reputation_action(&f.ctx, msg, &retry, f.ctx.keys())
            .await
            .unwrap();

        let replies = replies_to(retry.sender).await;
        assert_eq!(replies.len(), 1);
        let reply = replies[0].get_inner_message_kind();
        assert_eq!(reply.action, Action::ReputationImported);
        assert_eq!(reply.request_id, Some(7));
        assert!(reply.payload.is_none());
        let sender = retry.sender.to_hex();
        let published = crate::config::MESSAGE_QUEUES
            .queue_order_rate
            .read()
            .await
            .iter()
            .find(|ev| {
                ev.tags
                    .iter()
                    .any(|tag| tag.as_slice() == ["d", sender.as_str()])
            })
            .cloned()
            .expect("an expired committed import queues its current rating again");
        assert_eq!(published.pubkey, f.ctx.keys().public_key());
        assert!(published
            .tags
            .iter()
            .any(|tag| tag.as_slice() == ["total_reviews", "215"]));
        let after = is_user_present(f.ctx.pool(), identity.to_hex())
            .await
            .unwrap();
        assert_eq!(after, before);
        assert_eq!(
            reputation_imports_for_identity(f.ctx.pool(), &identity.to_hex())
                .await
                .unwrap(),
            rows
        );
    }

    #[tokio::test]
    async fn an_expired_retry_still_requires_the_exact_signed_event_and_identity() {
        let f = fixture().await;
        let identity = Keys::generate().public_key();
        let json = recorded_expired_attestation(&f, &identity).await;
        let signed = Event::from_json(&json).unwrap();
        let mut forged = signed.clone();
        forged.sig = EventBuilder::new(nostr_sdk::prelude::Kind::TextNote, "decoy")
            .finalize(&Keys::generate())
            .unwrap()
            .sig;
        assert_eq!(
            refused(import(&f, &identity, &forged.as_json()).await),
            CantDoReason::InvalidReputationAttestation
        );
        forged = signed.clone();
        forged.content = "changed without updating the id".into();
        assert_eq!(
            refused(import(&f, &identity, &forged.as_json()).await),
            CantDoReason::InvalidReputationAttestation
        );
        let other = Keys::generate().public_key();
        assert!(import(&f, &other, &json).await.is_err());
        let different = attestation_at(
            &f.issuer,
            &identity,
            "acct-1",
            Timestamp::from(signed.created_at.as_secs() - 60),
        );
        assert_eq!(
            refused(import(&f, &identity, &different).await),
            CantDoReason::ExpiredReputationAttestation
        );
        assert_eq!(
            is_user_present(f.ctx.pool(), identity.to_hex())
                .await
                .unwrap()
                .total_reviews,
            214
        );
    }

    #[tokio::test]
    async fn an_expired_retry_still_obeys_the_current_import_policy() {
        let f = fixture().await;
        let identity = Keys::generate().public_key();
        let json = recorded_expired_attestation(&f, &identity).await;
        for reason in [
            CantDoReason::InvalidAction,
            CantDoReason::UntrustedReputationIssuer,
            CantDoReason::InvalidReputationAttestation,
        ] {
            let mut settings = test_settings();
            settings.reputation_import = f.ctx.settings().reputation_import.clone();
            let policy = settings.reputation_import.as_mut().unwrap();
            match reason {
                CantDoReason::InvalidAction => policy.enabled = false,
                CantDoReason::UntrustedReputationIssuer => policy.issuers.clear(),
                _ => policy.max_lifetime_seconds = 1,
            }
            let ctx = TestContextBuilder::new()
                .with_pool(Arc::new(f.ctx.pool().clone()))
                .with_settings(settings)
                .build();
            let (msg, retry) = request(&identity, &json);
            assert_eq!(
                refused(import_reputation_action(&ctx, msg, &retry, ctx.keys()).await),
                reason
            );
            assert!(replies_to(retry.sender).await.is_empty());
        }
        let (msg, mut retry) = request(&identity, &json);
        retry.sender = identity;
        assert_eq!(
            refused(import_reputation_action(&f.ctx, msg, &retry, f.ctx.keys()).await),
            CantDoReason::ReputationIdentityRequired
        );
    }

    #[tokio::test]
    async fn an_expired_retry_cannot_restore_a_reversed_import() {
        let f = fixture().await;
        let identity = Keys::generate().public_key();
        let json = recorded_expired_attestation(&f, &identity).await;
        let rows = reputation_imports_for_identity(f.ctx.pool(), &identity.to_hex())
            .await
            .unwrap();
        let mut user = is_user_present(f.ctx.pool(), identity.to_hex())
            .await
            .unwrap();
        user.revert_reputation_import(&rows[0].figures().unwrap(), None);
        let mut tx = f.ctx.pool().begin().await.unwrap();
        update_user_reputation(&mut *tx, &user).await.unwrap();
        crate::db::delete_reputation_import(&mut *tx, &rows[0].attestation_id)
            .await
            .unwrap();
        tx.commit().await.unwrap();

        assert_eq!(
            refused(import(&f, &identity, &json).await),
            CantDoReason::ExpiredReputationAttestation
        );
        assert!(
            reputation_imports_for_identity(f.ctx.pool(), &identity.to_hex())
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            is_user_present(f.ctx.pool(), identity.to_hex())
                .await
                .unwrap()
                .total_reviews,
            0
        );
    }

    #[tokio::test]
    async fn a_rotated_issuer_key_cannot_reopen_an_imported_account() {
        let (old, new) = (Keys::generate(), Keys::generate());
        let keys = vec![old.public_key().to_hex(), new.public_key().to_hex()];
        let f = fixture_with(vec![("lnp2pbot", keys)], old.clone()).await;
        let identity = Keys::generate().public_key();
        import(&f, &identity, &attestation(&old, &identity, "acct-1"))
            .await
            .unwrap();
        assert_eq!(
            refused(import(&f, &identity, &attestation(&new, &identity, "acct-1")).await),
            CantDoReason::ReputationAlreadyImported
        );
    }

    #[tokio::test]
    async fn two_concurrent_imports_of_one_source_merge_once() {
        let f = fixture().await;
        let (a, b) = (Keys::generate().public_key(), Keys::generate().public_key());
        let (ja, jb) = (
            attestation(&f.issuer, &a, "acct-1"),
            attestation(&f.issuer, &b, "acct-1"),
        );
        let (ra, rb) = tokio::join!(import(&f, &a, &ja), import(&f, &b, &jb));
        assert_eq!([ra.is_ok(), rb.is_ok()].iter().filter(|ok| **ok).count(), 1);
        let failed = if ra.is_err() { ra } else { rb };
        assert_eq!(refused(failed), CantDoReason::ReputationAlreadyImported);
    }

    #[tokio::test]
    async fn each_redemption_check_refuses_with_its_reason() {
        let f = fixture().await;
        let identity = Keys::generate().public_key();
        let valid = attestation(&f.issuer, &identity, "acct-1");

        // No identity proof: full privacy mode.
        let (msg, mut event) = request(&identity, &valid);
        event.sender = identity;
        assert_eq!(
            refused(import_reputation_action(&f.ctx, msg, &event, f.ctx.keys()).await),
            CantDoReason::ReputationIdentityRequired
        );
        // Wrong payload.
        let (_, event) = request(&identity, &valid);
        let wrong = Message::new_order(None, None, None, Action::ImportReputation, None);
        assert_eq!(
            refused(import_reputation_action(&f.ctx, wrong, &event, f.ctx.keys()).await),
            CantDoReason::InvalidPayload
        );
        assert_eq!(
            refused(import(&f, &identity, "not an event").await),
            CantDoReason::InvalidReputationAttestation
        );
        let expired = ReputationAttestation::build(
            &f.issuer,
            &identity,
            "acct-1",
            214,
            4.87,
            SINCE,
            Timestamp::from(Timestamp::now().as_secs() - 2 * 86_400),
            86_400,
        )
        .unwrap()
        .as_json();
        assert_eq!(
            refused(import(&f, &identity, &expired).await),
            CantDoReason::ExpiredReputationAttestation
        );
        // Core refuses to build an attestation longer than
        // ATTESTATION_LIFETIME_SECS, so a node capped below that sees a
        // full-length one as too long.
        let mut settings = test_settings();
        settings.reputation_import =
            f.ctx
                .settings()
                .reputation_import
                .clone()
                .map(|import| ReputationImportSettings {
                    max_lifetime_seconds: ATTESTATION_LIFETIME_SECS - 1,
                    ..import
                });
        let capped = Fixture {
            ctx: TestContextBuilder::new()
                .with_pool(Arc::new(f.ctx.pool().clone()))
                .with_settings(settings)
                .build(),
            issuer: f.issuer.clone(),
        };
        assert_eq!(
            refused(import(&capped, &identity, &valid).await),
            CantDoReason::InvalidReputationAttestation
        );
        let stranger = Keys::generate();
        assert_eq!(
            refused(import(&f, &identity, &attestation(&stranger, &identity, "acct-1")).await),
            CantDoReason::UntrustedReputationIssuer
        );
        let other = Keys::generate().public_key();
        assert_eq!(
            refused(import(&f, &identity, &attestation(&f.issuer, &other, "acct-1")).await),
            CantDoReason::ReputationIdentityMismatch
        );
        assert!(
            reputation_imports_for_identity(f.ctx.pool(), &identity.to_hex())
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn a_node_that_does_not_import_refuses_the_action() {
        let mut f = fixture().await;
        let mut settings = test_settings();
        settings.reputation_import = Some(ReputationImportSettings::default());
        f.ctx = TestContextBuilder::new()
            .with_pool(Arc::new(f.ctx.pool().clone()))
            .with_settings(settings)
            .build();
        let identity = Keys::generate().public_key();
        assert_eq!(
            refused(import(&f, &identity, &attestation(&f.issuer, &identity, "acct-1")).await),
            CantDoReason::InvalidAction
        );
    }
}
