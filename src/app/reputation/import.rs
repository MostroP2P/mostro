//! `import-reputation`: a user imports an attestation another issuer signed
//! (docs/REPUTATION_PORTABILITY.md §5.3; MostroP2P/protocol
//! reputation_attestation.md, "Redemption").

use crate::app::context::AppContext;
use crate::app::rate_user::rating_event_tags;
use crate::config::types::ReputationImportSettings;
use crate::db::{insert_reputation_import, update_user_reputation, ReputationImportRow};
use crate::util::{enqueue_order_msg, update_user_rating_event};
use mostro_core::prelude::*;
use mostro_core::reputation::ReputationImport;
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
/// then confirm with `reputation-imported` and republish the user's rating
/// event with the merged figures. Every refusal is a `cant-do` whose reason
/// says which check failed.
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
    let (attestation, _) =
        ReputationAttestation::parse_json(json, Timestamp::now(), import.max_lifetime_seconds)
            .map_err(|e| MostroCantDo(e.cant_do_reason()))?;
    // Step 3: a trusted key, which from here on stands for its entry's name,
    // and never this node's own issuer key: importing its own attestations
    // would let a user double their reputation here.
    let own_key = ctx
        .settings()
        .reputation_export
        .as_ref()
        .filter(|export| export.enabled)
        .and_then(|export| export.issuer_key());
    if own_key == Some(attestation.issuer) {
        return Err(MostroCantDo(CantDoReason::UntrustedReputationIssuer));
    }
    let issuer = import
        .issuer_for(&attestation.issuer)
        .ok_or(MostroCantDo(CantDoReason::UntrustedReputationIssuer))?;
    // Step 5: addressed to the identity the transport proved.
    if attestation.destination != event.identity {
        return Err(MostroCantDo(CantDoReason::ReputationIdentityMismatch));
    }

    let user = record_import(ctx, &issuer.name, &attestation, event).await?;
    tracing::info!(
        "reputation: {} imported {} ratings from `{}` (attestation {})",
        event.identity,
        attestation.reviews,
        issuer.name,
        attestation.id
    );
    enqueue_order_msg(
        kind.request_id,
        None,
        Action::ReputationImported,
        None,
        event.sender,
        None,
    )
    .await;
    // Rating events are keyed by trade pubkey, like after a rating: the one
    // that sent the import shows the merged reputation from now on. Order
    // events published from now on read the merged row too.
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
        // The rating event reads the global settings (expiration), like every
        // handler test that publishes one; installing them is idempotent.
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
        ReputationAttestation::build(
            issuer,
            destination,
            subject,
            214,
            4.87,
            SINCE,
            Timestamp::now(),
            ATTESTATION_LIFETIME_SECS,
        )
        .unwrap()
        .as_json()
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
        import_reputation_action(&f.ctx, msg, &event, &Keys::generate()).await
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
        import_reputation_action(&f.ctx, msg, &event, &Keys::generate())
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

        let replies: Vec<Message> = crate::config::MESSAGE_QUEUES
            .queue_order_msg
            .read()
            .await
            .iter()
            .filter(|(_, pk)| *pk == event.sender)
            .map(|(m, _)| m.clone())
            .collect();
        assert_eq!(replies.len(), 1);
        let reply = replies[0].get_inner_message_kind();
        assert_eq!(reply.action, Action::ReputationImported);
        assert_eq!(reply.request_id, Some(7));
        assert!(reply.payload.is_none());
    }

    /// The rating event is republished under the importing trade key with
    /// the merged figures, so relays show the imported reputation at once.
    #[tokio::test]
    async fn an_import_republishes_the_rating_event_with_the_merged_figures() {
        let f = fixture().await;
        let identity = Keys::generate().public_key();
        let (msg, event) = request(&identity, &attestation(&f.issuer, &identity, "acct-1"));
        let node = Keys::generate();
        import_reputation_action(&f.ctx, msg, &event, &node)
            .await
            .unwrap();

        let tag = |ev: &Event, name: &str| {
            ev.tags.iter().find_map(|t| {
                let v = t.clone().to_vec();
                (v.first().map(String::as_str) == Some(name)).then(|| v[1].clone())
            })
        };
        let published: Vec<Event> = crate::config::MESSAGE_QUEUES
            .queue_order_rate
            .read()
            .await
            .iter()
            .filter(|ev| tag(ev, "d") == Some(event.sender.to_hex()))
            .cloned()
            .collect();
        assert_eq!(published.len(), 1);
        let ev = &published[0];
        assert_eq!(ev.pubkey, node.public_key());
        assert_eq!(ev.kind.as_u16(), NOSTR_RATING_EVENT_KIND);
        assert_eq!(tag(ev, "total_reviews"), Some("214".to_string()));
        assert_eq!(tag(ev, "since"), Some(SINCE.to_string()));
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
        // The same identity again, and the same account for another identity.
        assert_eq!(
            refused(import(&f, &a, &attestation(&f.issuer, &a, "acct-1")).await),
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
            refused(import_reputation_action(&f.ctx, msg, &event, &Keys::generate()).await),
            CantDoReason::ReputationIdentityRequired
        );
        // Wrong payload.
        let (_, event) = request(&identity, &valid);
        let wrong = Message::new_order(None, None, None, Action::ImportReputation, None);
        assert_eq!(
            refused(import_reputation_action(&f.ctx, wrong, &event, &Keys::generate()).await),
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
        let too_long = ReputationAttestation::build(
            &f.issuer,
            &identity,
            "acct-1",
            214,
            4.87,
            SINCE,
            Timestamp::now(),
            ATTESTATION_LIFETIME_SECS + 1,
        )
        .unwrap()
        .as_json();
        assert_eq!(
            refused(import(&f, &identity, &too_long).await),
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

    /// Step 3: even with its own issuer key in the trust list (settings
    /// loading refuses that, but the check stands on its own), a node never
    /// imports its own attestations.
    #[tokio::test]
    async fn the_nodes_own_attestations_are_never_imported() {
        let own = Keys::generate();
        let mut f =
            fixture_with(vec![("self", vec![own.public_key().to_hex()])], own.clone()).await;
        let mut settings = test_settings();
        settings.reputation_import = f.ctx.settings().reputation_import.clone();
        settings.reputation_export = Some(crate::config::types::ReputationExportSettings {
            enabled: true,
            issuer_keys: Some(own.clone()),
            ..Default::default()
        });
        f.ctx = TestContextBuilder::new()
            .with_pool(Arc::new(f.ctx.pool().clone()))
            .with_settings(settings)
            .build();
        let identity = Keys::generate().public_key();
        assert_eq!(
            refused(import(&f, &identity, &attestation(&own, &identity, "acct-1")).await),
            CantDoReason::UntrustedReputationIssuer
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
