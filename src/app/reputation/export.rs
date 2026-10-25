//! Issuing reputation attestations (docs/REPUTATION_PORTABILITY.md §5.2,
//! phase 4): what this node attests, and to whom.

use crate::app::context::AppContext;
use crate::config::types::ReputationExportSettings;
use crate::db::{
    bind_reputation_export, completed_trades_for_identity, find_user_by_pubkey,
    move_reputation_binding_once,
};
use crate::util::enqueue_order_msg;
use mostro_core::prelude::*;
use mostro_core::reputation::{MAX_CLOCK_SKEW_SECS, MIN_ATTESTED_SINCE};
use mostro_core::user::day_truncate;
use nostr_sdk::prelude::*;
use sqlx::SqlitePool;

/// Fewest completed trades an account needs to export its reputation.
pub const MIN_COMPLETED_TRADES: i64 = 10;
/// Fewest ratings received natively an account needs to export.
pub const MIN_NATIVE_REVIEWS: i64 = 5;

/// What an attestation from this node carries for an identity: its native
/// reputation alone, so an import never travels on (§7).
#[derive(Debug, Clone, PartialEq)]
pub struct NativeReputation {
    /// Ratings received here.
    pub reviews: i64,
    /// Their exact average.
    pub rating: f64,
    /// Completed trades here; decides eligibility, not carried.
    pub completed_trades: i64,
    /// UTC day start of the first completed trade here that started no
    /// earlier than [`MIN_ATTESTED_SINCE`]: an attestation cannot carry an
    /// older date.
    pub since: Option<u64>,
}

impl NativeReputation {
    /// Not banned, at least [`MIN_COMPLETED_TRADES`] completed trades and
    /// [`MIN_NATIVE_REVIEWS`] ratings received, all native, and a first
    /// trade on record.
    pub fn is_eligible(&self, user: &User) -> bool {
        user.is_banned == 0
            && self.completed_trades >= MIN_COMPLETED_TRADES
            && self.reviews >= MIN_NATIVE_REVIEWS
            && self.since.is_some()
    }
}

/// The export settings, only when export is enabled and its key is loaded.
pub fn export_settings(ctx: &AppContext) -> Option<(&ReputationExportSettings, &Keys)> {
    let export = ctx.settings().reputation_export.as_ref()?;
    let keys = export.issuer_keys.as_ref()?;
    export.enabled.then_some((export, keys))
}

/// `export-reputation`: attest the requester's native reputation on this
/// node for the destination identity they name (§5.2). The source account is
/// the identity the transport proved, never the payload. The attestation is
/// returned only once the account is bound to the destination with a
/// compare-and-set: a request for the identity already bound is how a lost,
/// expired or fresher attestation is obtained; one for another identity is
/// refused unless it carries a rebind authorisation from the bound identity
/// ([`apply_rebind`]). A failure to sign binds nothing. The client asked
/// the user to confirm the destination before sending the request, which
/// the source identity signs.
pub async fn export_reputation_action(
    ctx: &AppContext,
    msg: Message,
    event: &UnwrappedMessage,
) -> Result<(), MostroError> {
    let (export, issuer) = export_settings(ctx).ok_or(MostroCantDo(CantDoReason::InvalidAction))?;
    let kind = msg.get_inner_message_kind();
    let Some(Payload::ReputationExportRequest(request)) = kind.get_payload() else {
        return Err(MostroCantDo(CantDoReason::InvalidPayload));
    };
    if event.identity == event.sender {
        return Err(MostroCantDo(CantDoReason::ReputationIdentityRequired));
    }
    let destination = parse_destination(&request.destination)?;

    let identity = event.identity.to_hex();
    let not_eligible = || MostroCantDo(CantDoReason::NotEligibleForReputationExport);
    let user = find_user_by_pubkey(ctx.pool(), identity.clone())
        .await?
        .ok_or_else(not_eligible)?;
    let native = native_reputation(ctx.pool(), &user).await?;
    if !native.is_eligible(&user) {
        return Err(not_eligible());
    }
    let since = native.since.ok_or_else(not_eligible)?;

    // Signed before the account is bound, and returned only once it is: a
    // failure to sign must not bind the account to an identity that never
    // received an attestation.
    let now = Timestamp::now();
    let attestation = ReputationAttestation::build(
        issuer,
        &destination,
        &identity,
        u32::try_from(native.reviews).unwrap_or(u32::MAX),
        native.rating,
        since,
        now,
        export.lifetime_seconds,
    )
    .map_err(|e| MostroInternalErr(ServiceError::UnexpectedError(e.to_string())))?;
    let today = day_truncate(now.as_secs() as i64) as i64;
    let bound = bind_reputation_export(ctx.pool(), &identity, &destination.to_hex(), today).await?;
    if !bound {
        // Bound to another identity: only a rebind authorisation from it
        // moves the binding.
        let Some(rebind) = request.rebind.as_deref() else {
            return Err(MostroCantDo(CantDoReason::ReputationBoundToOtherIdentity));
        };
        apply_rebind(
            ctx,
            issuer,
            &identity,
            &destination,
            rebind,
            now,
            &event.sender,
        )
        .await?;
    }
    // Name the trade key, as the import handler does: the identity and the
    // destination together would tie two long-lived identities in the logs.
    tracing::info!(
        "reputation: trade key {} exported {} ratings (attestation {})",
        event.sender,
        native.reviews,
        attestation.id
    );
    enqueue_order_msg(
        kind.request_id,
        None,
        Action::ReputationExported,
        Some(Payload::ReputationAttestation(attestation.as_json())),
        event.sender,
        None,
    )
    .await;
    Ok(())
}

/// Move the binding with a rebind authorisation (§5.2, "Rebinding"): signed
/// by the identity the account is bound to, naming this issuer and the
/// requested destination, still valid, and not applied before. The move is
/// a compare-and-set conditioned on that identity, so two racing moves
/// cannot both apply, and it records the authorisation, so a replay is
/// refused even once the binding is back at its signer. The account is read
/// here, after the first binding failed, so the move starts from the
/// binding as it is now.
async fn apply_rebind(
    ctx: &AppContext,
    issuer: &Keys,
    identity: &str,
    destination: &PublicKey,
    rebind: &str,
    now: Timestamp,
    trade_key: &PublicKey,
) -> Result<(), MostroError> {
    let invalid = || MostroCantDo(CantDoReason::InvalidReputationRebind);
    let user = find_user_by_pubkey(ctx.pool(), identity.to_string())
        .await?
        .ok_or(MostroCantDo(CantDoReason::NotEligibleForReputationExport))?;
    let rebind = ReputationRebind::parse_json(rebind, now).map_err(|_| invalid())?;
    let bound = rebind.bound_identity.to_hex();
    let valid = rebind.issuer == issuer.public_key()
        && rebind.new_identity == *destination
        && user.reputation_exported_to.as_deref() == Some(bound.as_str());
    if !valid {
        return Err(invalid());
    }
    let today = day_truncate(now.as_secs() as i64) as i64;
    // Core accepts it until its expiration plus the clock skew allowance.
    let valid_until = rebind.expiration.saturating_add(MAX_CLOCK_SKEW_SECS);
    let moved = move_reputation_binding_once(
        ctx.pool(),
        &user.pubkey,
        &bound,
        &destination.to_hex(),
        today,
        &rebind.id.to_hex(),
        i64::try_from(valid_until).unwrap_or(i64::MAX),
    )
    .await?;
    if !moved {
        return Err(invalid());
    }
    // Name the trade key and the authorisation: the account and both
    // bindings would tie three long-lived identities in the logs. Only the
    // operator's move is logged with both identities (§5.2).
    tracing::warn!(
        "reputation: trade key {} moved its export binding (rebind authorisation {})",
        trade_key,
        rebind.id
    );
    Ok(())
}

/// The destination identity: 64 lowercase hex characters, a valid key.
fn parse_destination(text: &str) -> Result<PublicKey, MostroError> {
    let lowercase_hex = text.len() == 64
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    lowercase_hex
        .then(|| PublicKey::from_hex(text).ok())
        .flatten()
        .ok_or(MostroCantDo(CantDoReason::InvalidPayload))
}

/// An identity's native reputation on this node, from its user row and its
/// orders.
pub async fn native_reputation(
    pool: &SqlitePool,
    user: &User,
) -> Result<NativeReputation, MostroError> {
    let (reviews, rating) = user.native_stats();
    let (completed_trades, first_trade) =
        completed_trades_for_identity(pool, &user.pubkey, MIN_ATTESTED_SINCE as i64).await?;
    Ok(NativeReputation {
        reviews,
        rating,
        completed_trades,
        since: first_trade.map(day_truncate),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use mostro_core::db::Crud;
    use mostro_core::order::Order;
    use mostro_core::reputation::ReputationImport;
    use nostr_sdk::prelude::Keys;
    use uuid::Uuid;

    async fn pool() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!().run(&pool).await.unwrap();
        pool
    }

    async fn order(pool: &SqlitePool, identity: &str, buyer: bool, status: Status, taken_at: i64) {
        let mut order = Order {
            id: Uuid::new_v4(),
            status: status.to_string(),
            kind: mostro_core::order::Kind::Sell.to_string(),
            fiat_code: "USD".to_string(),
            creator_pubkey: Keys::generate().public_key().to_string(),
            amount: 10_000,
            created_at: 1_700_000_000,
            taken_at,
            ..Default::default()
        };
        if buyer {
            order.master_buyer_pubkey = Some(identity.to_string());
        } else {
            order.master_seller_pubkey = Some(identity.to_string());
        }
        order.create(pool).await.unwrap();
    }

    fn rated(identity: &str, ratings: &[u8]) -> User {
        let mut user = User::new(identity.to_string(), 0, 0, 0, 0, 1);
        for rating in ratings {
            user.update_rating(*rating);
        }
        user
    }

    #[tokio::test]
    async fn counts_success_orders_on_either_side_and_dates_the_first() {
        let pool = pool().await;
        let identity = Keys::generate().public_key().to_hex();
        order(&pool, &identity, true, Status::Success, 1_700_090_000).await;
        order(&pool, &identity, false, Status::Success, 0).await;
        order(&pool, &identity, true, Status::Canceled, 1_600_000_000).await;
        order(
            &pool,
            &Keys::generate().public_key().to_hex(),
            true,
            Status::Success,
            1,
        )
        .await;
        let native = native_reputation(&pool, &rated(&identity, &[5]))
            .await
            .unwrap();
        assert_eq!(native.completed_trades, 2);
        // The order without taken_at falls back to its creation, the earlier.
        assert_eq!(native.since, Some(day_truncate(1_700_000_000)));
        assert_eq!((native.reviews, native.rating), (1, 5.0));
    }

    #[tokio::test]
    async fn eligibility_needs_ten_trades_five_native_ratings_and_no_ban() {
        let pool = pool().await;
        let identity = Keys::generate().public_key().to_hex();
        for _ in 0..10 {
            order(&pool, &identity, true, Status::Success, 1_700_000_000).await;
        }
        let user = rated(&identity, &[5, 4, 5, 5, 3]);
        let native = native_reputation(&pool, &user).await.unwrap();
        assert!(native.is_eligible(&user));

        let banned = User {
            is_banned: 1,
            ..user.clone()
        };
        assert!(!native.is_eligible(&banned));
        let four = rated(&identity, &[5, 4, 5, 5]);
        assert!(!native_reputation(&pool, &four)
            .await
            .unwrap()
            .is_eligible(&four));
        let nine = NativeReputation {
            completed_trades: 9,
            ..native.clone()
        };
        assert!(!nine.is_eligible(&user));
    }

    /// An attestation cannot carry a `since` before `MIN_ATTESTED_SINCE`, and
    /// an order stored while the node's clock read 1970 has nothing better:
    /// such a trade counts, does not hide a later first trade, and alone does
    /// not make an identity eligible.
    #[tokio::test]
    async fn a_first_trade_dated_before_attestations_exist_is_ignored() {
        let pool = pool().await;
        let identity = Keys::generate().public_key().to_hex();
        for _ in 0..9 {
            order(&pool, &identity, true, Status::Success, 1_700_000_000).await;
        }
        order(&pool, &identity, true, Status::Success, 1_000).await;
        let user = rated(&identity, &[5, 5, 5, 5, 5]);
        let native = native_reputation(&pool, &user).await.unwrap();
        assert_eq!(native.completed_trades, 10);
        assert_eq!(native.since, Some(day_truncate(1_700_000_000)));
        assert!(native.is_eligible(&user));

        let wrong_clock = Keys::generate().public_key().to_hex();
        for _ in 0..10 {
            order(&pool, &wrong_clock, true, Status::Success, 1_000).await;
        }
        let user = rated(&wrong_clock, &[5, 5, 5, 5, 5]);
        let native = native_reputation(&pool, &user).await.unwrap();
        assert_eq!(native.completed_trades, 10);
        assert_eq!(native.since, None);
        assert!(!native.is_eligible(&user));
    }

    /// The lookup goes through the two master-key indexes. Walking the
    /// `status` index instead reads every `success` order on each export
    /// request.
    #[tokio::test]
    async fn the_completed_trades_lookup_uses_both_master_key_indexes() {
        let pool = pool().await;
        let plan: Vec<String> = sqlx::query_as::<_, (i64, i64, i64, String)>(sqlx::AssertSqlSafe(
            format!("EXPLAIN QUERY PLAN {}", crate::db::COMPLETED_TRADES_QUERY),
        ))
        .bind("identity")
        .bind(0_i64)
        .fetch_all(&pool)
        .await
        .unwrap()
        .into_iter()
        .map(|(_, _, _, detail)| detail)
        .collect();
        for index in [
            "idx_orders_master_buyer_pubkey",
            "idx_orders_master_seller_pubkey",
        ] {
            assert!(plan.iter().any(|d| d.contains(index)), "{plan:?}");
        }
    }

    #[tokio::test]
    async fn imported_ratings_never_count_or_travel() {
        let pool = pool().await;
        let identity = Keys::generate().public_key().to_hex();
        for _ in 0..10 {
            order(&pool, &identity, true, Status::Success, 1_700_000_000).await;
        }
        let mut user = rated(&identity, &[5, 5]);
        user.apply_reputation_import(&ReputationImport {
            reviews: 200,
            rating_hundredths: 300,
            since: 1_600_000_000 - 1_600_000_000 % 86_400,
        });
        let native = native_reputation(&pool, &user).await.unwrap();
        assert_eq!((native.reviews, native.rating), (2, 5.0));
        assert!(
            !native.is_eligible(&user),
            "two native ratings are not enough"
        );
        // The imported date is not native either: the first trade here is.
        assert_eq!(native.since, Some(day_truncate(1_700_000_000)));
    }
}

#[cfg(test)]
mod handler_tests {
    use super::*;
    use crate::app::context::test_utils::{self, test_settings, TestContextBuilder};
    use crate::app::reputation::import::import_reputation_action;
    use crate::config::types::{ReputationImportSettings, ReputationIssuer};
    use crate::db::{
        add_new_user, is_user_present, move_reputation_binding, move_reputation_binding_once,
    };
    use mostro_core::db::Crud;
    use mostro_core::order::Order;
    use mostro_core::reputation::ATTESTATION_LIFETIME_SECS;
    use sqlx::sqlite::SqlitePoolOptions;
    use std::sync::Arc;
    use uuid::Uuid;

    async fn pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        pool
    }

    /// Node A, which exports with `issuer`.
    async fn issuer_node(issuer: &Keys) -> AppContext {
        issuer_node_with(issuer, ATTESTATION_LIFETIME_SECS).await
    }

    /// Node A, issuing attestations that live `lifetime_seconds`.
    async fn issuer_node_with(issuer: &Keys, lifetime_seconds: u64) -> AppContext {
        let _ = crate::config::MOSTRO_CONFIG.set(test_settings());
        let mut settings = test_settings();
        settings.reputation_export = Some(ReputationExportSettings {
            enabled: true,
            issuer_keys: Some(issuer.clone()),
            lifetime_seconds,
            ..Default::default()
        });
        TestContextBuilder::new()
            .with_pool(Arc::new(pool().await))
            .with_settings(settings)
            .build()
    }

    /// Node B, which imports from `issuer`.
    async fn destination_node(issuer: &Keys) -> AppContext {
        let mut settings = test_settings();
        settings.reputation_import = Some(ReputationImportSettings {
            enabled: true,
            issuers: vec![ReputationIssuer {
                name: "node-a".to_string(),
                keys: vec![issuer.public_key().to_hex()],
            }],
            ..Default::default()
        });
        TestContextBuilder::new()
            .with_pool(Arc::new(pool().await))
            .with_settings(settings)
            .build()
    }

    /// A node that exports with `own` and imports from `other`, trusted as
    /// `other_name`: one of two nodes that carry reputation both ways.
    async fn two_way_node(own: &Keys, other_name: &str, other: &Keys) -> AppContext {
        let _ = crate::config::MOSTRO_CONFIG.set(test_settings());
        let mut settings = test_settings();
        settings.reputation_export = Some(ReputationExportSettings {
            enabled: true,
            issuer_keys: Some(own.clone()),
            lifetime_seconds: ATTESTATION_LIFETIME_SECS,
            ..Default::default()
        });
        settings.reputation_import = Some(ReputationImportSettings {
            enabled: true,
            issuers: vec![ReputationIssuer {
                name: other_name.to_string(),
                keys: vec![other.public_key().to_hex()],
            }],
            ..Default::default()
        });
        TestContextBuilder::new()
            .with_pool(Arc::new(pool().await))
            .with_settings(settings)
            .build()
    }

    /// Nodes A and B, each exporting with its own key and importing from
    /// the other's.
    async fn nodes_both_ways() -> (AppContext, AppContext) {
        let (a_key, b_key) = (Keys::generate(), Keys::generate());
        let a = two_way_node(&a_key, "node-b", &b_key).await;
        let b = two_way_node(&b_key, "node-a", &a_key).await;
        (a, b)
    }

    /// An identity with 10 completed trades and native ratings 5, 4, 5, 5, 3
    /// on `ctx`'s node, first trade taken on 2023-10-02.
    async fn eligible_identity(ctx: &AppContext) -> Keys {
        identity_with(ctx, 10, &[5, 4, 5, 5, 3]).await
    }

    /// An identity with `trades` completed trades and the given native
    /// ratings on `ctx`'s node, first trade taken on 2023-10-02. Its row is
    /// dated before that trade, as a real one is: `add_new_user` stamps it
    /// with the test's clock.
    async fn identity_with(ctx: &AppContext, trades: i64, ratings: &[u8]) -> Keys {
        let identity = Keys::generate();
        let hex = identity.public_key().to_hex();
        add_new_user(ctx.pool(), User::new(hex.clone(), 0, 0, 0, 0, 1))
            .await
            .unwrap();
        sqlx::query("UPDATE users SET created_at = ?2, native_created_at = ?2 WHERE pubkey = ?1")
            .bind(&hex)
            .bind(1_696_200_000_i64)
            .execute(ctx.pool())
            .await
            .unwrap();
        trade_and_rate(ctx, &identity.public_key(), trades, 1_696_250_000, ratings).await;
        identity
    }

    /// Add `trades` completed trades, the first taken at `first_taken_at`,
    /// and the given native ratings to `identity`'s row on `ctx`'s node.
    async fn trade_and_rate(
        ctx: &AppContext,
        identity: &PublicKey,
        trades: i64,
        first_taken_at: i64,
        ratings: &[u8],
    ) {
        let hex = identity.to_hex();
        let mut user = is_user_present(ctx.pool(), hex.clone()).await.unwrap();
        for rating in ratings {
            user.update_rating(*rating);
        }
        crate::db::update_user_rating(
            ctx.pool(),
            hex.clone(),
            user.last_rating,
            user.min_rating,
            user.max_rating,
            user.total_reviews,
            user.total_rating,
            user.native_rating_sum,
        )
        .await
        .unwrap();
        for i in 0..trades {
            Order {
                id: Uuid::new_v4(),
                status: Status::Success.to_string(),
                kind: mostro_core::order::Kind::Sell.to_string(),
                fiat_code: "USD".to_string(),
                creator_pubkey: Keys::generate().public_key().to_string(),
                amount: 10_000,
                created_at: first_taken_at - 50_000,
                taken_at: first_taken_at + i,
                master_seller_pubkey: Some(hex.clone()),
                ..Default::default()
            }
            .create(ctx.pool())
            .await
            .unwrap();
        }
    }

    fn request(identity: &PublicKey, destination: &str) -> (Message, UnwrappedMessage) {
        let msg = Message::new_order(
            None,
            Some(11),
            None,
            Action::ExportReputation,
            Some(Payload::ReputationExportRequest(ReputationExportRequest {
                destination: destination.to_string(),
                rebind: None,
            })),
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

    async fn export(
        ctx: &AppContext,
        identity: &PublicKey,
        destination: &PublicKey,
    ) -> Result<String, MostroError> {
        let (msg, event) = request(identity, &destination.to_hex());
        export_reputation_action(ctx, msg, &event).await?;
        let replies = crate::config::MESSAGE_QUEUES.queue_order_msg.read().await;
        let (reply, _) = replies
            .iter()
            .find(|(_, pk)| *pk == event.sender)
            .expect("a reply to the requesting trade key");
        let kind = reply.get_inner_message_kind();
        assert_eq!(kind.action, Action::ReputationExported);
        assert_eq!(kind.request_id, Some(11));
        match kind.get_payload() {
            Some(Payload::ReputationAttestation(json)) => Ok(json.clone()),
            other => panic!("unexpected payload {other:?}"),
        }
    }

    fn refused<T: std::fmt::Debug>(result: Result<T, MostroError>) -> CantDoReason {
        match result {
            Err(MostroCantDo(reason)) => reason,
            other => panic!("expected a cant-do, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn exports_the_native_reputation_for_the_destination_and_binds_it() {
        let issuer = Keys::generate();
        let ctx = issuer_node(&issuer).await;
        let identity = eligible_identity(&ctx).await.public_key();
        let destination = Keys::generate().public_key();
        let today = || day_truncate(Timestamp::now().as_secs() as i64) as i64;
        let first_day = today();
        let json = export(&ctx, &identity, &destination).await.unwrap();
        let last_day = today();

        let (attestation, _) =
            ReputationAttestation::parse_json(&json, Timestamp::now(), ATTESTATION_LIFETIME_SECS)
                .unwrap();
        assert_eq!(attestation.issuer, issuer.public_key());
        assert_eq!(attestation.destination, destination);
        assert_eq!(attestation.subject, identity.to_hex());
        assert_eq!(attestation.reviews, 5);
        assert_eq!(attestation.rating_text(), "4.40");
        assert_eq!(attestation.since, 1_696_204_800);
        let user = is_user_present(ctx.pool(), identity.to_hex())
            .await
            .unwrap();
        assert_eq!(user.reputation_exported_to, Some(destination.to_hex()));
        // The day of the export, whichever side of midnight it fell on.
        let day = user.reputation_exported_at.unwrap();
        assert!(day == first_day || day == last_day, "{day}");
    }

    /// `lifetime_seconds` is the attestation's lifetime.
    #[tokio::test]
    async fn the_configured_lifetime_is_the_attestations_lifetime() {
        let issuer = Keys::generate();
        let ctx = issuer_node_with(&issuer, 3_600).await;
        let identity = eligible_identity(&ctx).await.public_key();
        let json = export(&ctx, &identity, &Keys::generate().public_key())
            .await
            .unwrap();
        let (attestation, _) =
            ReputationAttestation::parse_json(&json, Timestamp::now(), ATTESTATION_LIFETIME_SECS)
                .unwrap();
        assert_eq!(attestation.expiration - attestation.created_at, 3_600);
    }

    /// Each floor refuses on its own. Every one of these identities has a
    /// first trade on record, so the missing-`since` check cannot be what
    /// refuses them.
    #[tokio::test]
    async fn every_floor_refuses_on_its_own() {
        let issuer = Keys::generate();
        let ctx = issuer_node(&issuer).await;
        let destination = Keys::generate().public_key();
        let cases: [(i64, &[u8], bool); 3] = [
            (9, &[5, 4, 5, 5, 3], false), // one trade short
            (10, &[5, 4, 5, 5], false),   // one rating short
            (10, &[5, 4, 5, 5, 3], true), // banned
        ];
        for (trades, ratings, banned) in cases {
            let identity = identity_with(&ctx, trades, ratings).await.public_key();
            if banned {
                sqlx::query("UPDATE users SET is_banned = 1 WHERE pubkey = ?1")
                    .bind(identity.to_hex())
                    .execute(ctx.pool())
                    .await
                    .unwrap();
            }
            assert_eq!(
                refused(export(&ctx, &identity, &destination).await),
                CantDoReason::NotEligibleForReputationExport,
                "{trades} trades, {} ratings, banned: {banned}",
                ratings.len()
            );
        }
    }

    #[tokio::test]
    async fn re_exports_to_the_bound_identity_and_refuses_another() {
        let issuer = Keys::generate();
        let ctx = issuer_node(&issuer).await;
        let identity = eligible_identity(&ctx).await.public_key();
        let destination = Keys::generate().public_key();
        export(&ctx, &identity, &destination).await.unwrap();
        export(&ctx, &identity, &destination).await.unwrap();
        assert_eq!(
            refused(export(&ctx, &identity, &Keys::generate().public_key()).await),
            CantDoReason::ReputationBoundToOtherIdentity
        );
    }

    /// A database failure is an operational failure, not a verdict on the
    /// user: it must not reach them as `not_eligible_for_reputation_export`.
    #[tokio::test]
    async fn a_database_failure_is_not_reported_as_not_eligible() {
        let issuer = Keys::generate();
        let ctx = issuer_node(&issuer).await;
        let identity = eligible_identity(&ctx).await.public_key();
        sqlx::query("ALTER TABLE users RENAME TO users_gone")
            .execute(ctx.pool())
            .await
            .unwrap();
        let (msg, event) = request(&identity, &Keys::generate().public_key().to_hex());
        let result = export_reputation_action(&ctx, msg, &event).await;
        assert!(
            matches!(
                result,
                Err(MostroInternalErr(ServiceError::DbAccessError(_)))
            ),
            "{result:?}"
        );
    }

    /// The log line of a successful export names the trade key and the
    /// attestation, never the identity nor the destination: together they
    /// would tie two long-lived identities in the logs.
    #[tokio::test]
    async fn the_export_log_does_not_pair_the_identity_with_the_destination() {
        let issuer = Keys::generate();
        let ctx = issuer_node(&issuer).await;
        let identity = eligible_identity(&ctx).await.public_key();
        let destination = Keys::generate().public_key();
        let (json, logs) =
            test_utils::logged_on_this_thread(export(&ctx, &identity, &destination)).await;
        let json = json.unwrap();

        let (attestation, _) =
            ReputationAttestation::parse_json(&json, Timestamp::now(), ATTESTATION_LIFETIME_SECS)
                .unwrap();
        assert!(logs.contains(&attestation.id.to_string()), "{logs}");
        assert!(!logs.contains(&identity.to_hex()), "{logs}");
        assert!(!logs.contains(&destination.to_hex()), "{logs}");
    }

    /// If the attestation cannot be signed, nothing is bound: the account
    /// stays free to export to the identity the user actually wants.
    #[tokio::test]
    async fn a_failed_signature_does_not_leave_the_account_bound() {
        let issuer = Keys::generate();
        // Longer than an attestation can carry, so signing fails. Startup
        // refuses this setting; any later failure to sign is the same case.
        let ctx = issuer_node_with(&issuer, ATTESTATION_LIFETIME_SECS + 1).await;
        let identity = eligible_identity(&ctx).await.public_key();
        let (msg, event) = request(&identity, &Keys::generate().public_key().to_hex());
        let result = export_reputation_action(&ctx, msg, &event).await;
        assert!(
            matches!(
                result,
                Err(MostroInternalErr(ServiceError::UnexpectedError(_)))
            ),
            "{result:?}"
        );
        let user = is_user_present(ctx.pool(), identity.to_hex())
            .await
            .unwrap();
        assert_eq!(user.reputation_exported_to, None);
        assert_eq!(user.reputation_exported_at, None);
    }

    #[tokio::test]
    async fn exactly_one_of_two_concurrent_bindings_wins() {
        let issuer = Keys::generate();
        let ctx = issuer_node(&issuer).await;
        let identity = eligible_identity(&ctx).await.public_key();
        let (x, y) = (Keys::generate().public_key(), Keys::generate().public_key());
        let (rx, ry) = tokio::join!(export(&ctx, &identity, &x), export(&ctx, &identity, &y));
        assert_eq!([rx.is_ok(), ry.is_ok()].iter().filter(|ok| **ok).count(), 1);
        let lost = if rx.is_err() { rx } else { ry };
        assert_eq!(refused(lost), CantDoReason::ReputationBoundToOtherIdentity);
    }

    #[tokio::test]
    async fn refuses_with_the_reason_of_each_check() {
        let issuer = Keys::generate();
        let ctx = issuer_node(&issuer).await;
        let identity = eligible_identity(&ctx).await.public_key();
        let destination = Keys::generate().public_key();

        let (msg, mut event) = request(&identity, &destination.to_hex());
        event.sender = identity;
        assert_eq!(
            refused(export_reputation_action(&ctx, msg, &event).await),
            CantDoReason::ReputationIdentityRequired
        );
        // `PublicKey::from_hex` accepts 128 hex characters, so the length
        // check is what refuses a key followed by anything.
        let too_long = format!("{0}{0}", destination.to_hex());
        for bad in ["", "AB", &destination.to_hex().to_uppercase(), &too_long] {
            let (msg, event) = request(&identity, bad);
            assert_eq!(
                refused(export_reputation_action(&ctx, msg, &event).await),
                CantDoReason::InvalidPayload
            );
        }
        let (_, event) = request(&identity, &destination.to_hex());
        let no_payload = Message::new_order(None, None, None, Action::ExportReputation, None);
        assert_eq!(
            refused(export_reputation_action(&ctx, no_payload, &event).await),
            CantDoReason::InvalidPayload
        );
        // A stranger, and an identity below the floors.
        assert_eq!(
            refused(export(&ctx, &Keys::generate().public_key(), &destination).await),
            CantDoReason::NotEligibleForReputationExport
        );
        let newcomer = Keys::generate().public_key();
        add_new_user(ctx.pool(), User::new(newcomer.to_hex(), 0, 0, 0, 0, 1))
            .await
            .unwrap();
        assert_eq!(
            refused(export(&ctx, &newcomer, &destination).await),
            CantDoReason::NotEligibleForReputationExport
        );
        // A node that does not export.
        let off = TestContextBuilder::new()
            .with_pool(Arc::new(pool().await))
            .with_settings(test_settings())
            .build();
        assert_eq!(
            refused(export(&off, &identity, &destination).await),
            CantDoReason::InvalidAction
        );
    }

    /// The plan's round trip: export from node A, import on node B, both in
    /// process. B ends up with A's native figures, counts a resent
    /// attestation once, and refuses a re-issued one.
    #[tokio::test]
    async fn an_export_from_one_node_imports_on_another() {
        let issuer = Keys::generate();
        let a = issuer_node(&issuer).await;
        let b = destination_node(&issuer).await;
        let identity = eligible_identity(&a).await.public_key();
        let json = export(&a, &identity, &identity).await.unwrap();

        let import = |json: String| {
            let msg = Message::new_order(
                None,
                None,
                None,
                Action::ImportReputation,
                Some(Payload::ReputationAttestation(json)),
            );
            let event = UnwrappedMessage {
                message: msg.clone(),
                signature: None,
                sender: Keys::generate().public_key(),
                identity,
                created_at: Timestamp::now(),
            };
            (msg, event)
        };
        let (msg, event) = import(json.clone());
        import_reputation_action(&b, msg, &event, &Keys::generate())
            .await
            .unwrap();
        let on_b = is_user_present(b.pool(), identity.to_hex()).await.unwrap();
        assert_eq!(on_b.total_reviews, 5);
        assert_eq!(on_b.total_rating, 4.4);
        assert_eq!(on_b.created_at, 1_696_204_800);
        assert_eq!(on_b.native_stats(), (0, 0.0), "nothing native on B");

        // The exact same attestation again is a replay: answered, counted once.
        let (msg, event) = import(json);
        import_reputation_action(&b, msg, &event, &Keys::generate())
            .await
            .unwrap();
        let on_b = is_user_present(b.pool(), identity.to_hex()).await.unwrap();
        assert_eq!(on_b.total_reviews, 5);

        // A re-issued attestation (a new id) is refused. A sixth rating on A
        // changes the attested content, so the id differs without waiting.
        let mut user = is_user_present(a.pool(), identity.to_hex()).await.unwrap();
        user.update_rating(5);
        crate::db::update_user_rating(
            a.pool(),
            identity.to_hex(),
            user.last_rating,
            user.min_rating,
            user.max_rating,
            user.total_reviews,
            user.total_rating,
            user.native_rating_sum,
        )
        .await
        .unwrap();
        let reissued = export(&a, &identity, &identity).await.unwrap();
        let (msg, event) = import(reissued);
        assert_eq!(
            refused(import_reputation_action(&b, msg, &event, &Keys::generate()).await),
            CantDoReason::ReputationAlreadyImported
        );
    }

    /// Send `json` to `ctx`'s node as `identity`'s `import-reputation`,
    /// through a fresh trade key.
    async fn import_on(
        ctx: &AppContext,
        identity: &PublicKey,
        json: String,
    ) -> Result<(), MostroError> {
        let msg = Message::new_order(
            None,
            None,
            None,
            Action::ImportReputation,
            Some(Payload::ReputationAttestation(json)),
        );
        let event = UnwrappedMessage {
            message: msg.clone(),
            signature: None,
            sender: Keys::generate().public_key(),
            identity: *identity,
            created_at: Timestamp::now(),
        };
        import_reputation_action(ctx, msg, &event, &Keys::generate()).await
    }

    /// Export `identity`'s reputation on `from` for the same identity and
    /// import it on `to`.
    async fn carry(
        from: &AppContext,
        to: &AppContext,
        identity: &PublicKey,
    ) -> Result<(), MostroError> {
        let json = export(from, identity, identity).await?;
        import_on(to, identity, json).await
    }

    fn attested(json: &str) -> ReputationAttestation {
        ReputationAttestation::parse_json(json, Timestamp::now(), ATTESTATION_LIFETIME_SECS)
            .unwrap()
            .0
    }

    /// When an identity's trading on B starts: a week before the test runs.
    fn a_week_ago() -> i64 {
        Timestamp::now().as_secs() as i64 - 7 * 86_400
    }

    /// Ratings carried from A count on B, but never towards exporting from
    /// B: only B's own trades and ratings do, so its floors are met there or
    /// not at all.
    #[tokio::test]
    async fn ratings_imported_from_a_never_make_b_export() {
        let (a, b) = nodes_both_ways().await;
        let identity = eligible_identity(&a).await.public_key();
        carry(&a, &b, &identity).await.unwrap();
        assert_eq!(
            refused(export(&b, &identity, &identity).await),
            CantDoReason::NotEligibleForReputationExport
        );

        // Ten trades and four ratings on B: nine ratings in all, four native.
        trade_and_rate(&b, &identity, 10, a_week_ago(), &[5, 5, 4, 5]).await;
        assert_eq!(
            refused(export(&b, &identity, &identity).await),
            CantDoReason::NotEligibleForReputationExport
        );
        trade_and_rate(&b, &identity, 0, 0, &[5]).await;
        export(&b, &identity, &identity).await.unwrap();
    }

    /// The way back: an identity that carried its reputation from A to B and
    /// traded on B for a week imports what it earned there into A. B attests
    /// its native figures only, so A adds the week and not its own ratings a
    /// second time, and keeps its native figures and its date.
    #[tokio::test]
    async fn reputation_earned_on_b_imports_back_into_a() {
        let (a, b) = nodes_both_ways().await;
        let identity = eligible_identity(&a).await.public_key();
        carry(&a, &b, &identity).await.unwrap();
        let week_ago = a_week_ago();
        trade_and_rate(&b, &identity, 10, week_ago, &[5, 5, 4, 5, 5]).await;
        let before = is_user_present(a.pool(), identity.to_hex()).await.unwrap();

        let json = export(&b, &identity, &identity).await.unwrap();
        let attestation = attested(&json);
        let (_, b_issuer) = export_settings(&b).unwrap();
        assert_eq!(attestation.issuer, b_issuer.public_key());
        assert_eq!(attestation.subject, identity.to_hex());
        assert_eq!(attestation.reviews, 5, "B's ratings, not the ten it shows");
        assert_eq!(attestation.rating_text(), "4.80");
        assert_eq!(attestation.since, day_truncate(week_ago));

        import_on(&a, &identity, json).await.unwrap();
        let after = is_user_present(a.pool(), identity.to_hex()).await.unwrap();
        assert_eq!(after.total_reviews, 10);
        let merged = (before.total_rating * 5.0 + 4.8 * 5.0) / 10.0;
        assert!(
            (after.total_rating - merged).abs() < 1e-9,
            "{}",
            after.total_rating
        );
        assert_eq!(after.native_stats(), before.native_stats());
        assert_eq!(
            after.created_at, before.created_at,
            "A's first trade is older"
        );
    }

    /// After the way back no rating counts twice on either node. A's next
    /// attestation carries A's ratings only, and B refuses it as a second
    /// import of the account; B's next one is refused by A the same way,
    /// since a source is imported once.
    #[tokio::test]
    async fn after_the_way_back_no_rating_counts_twice() {
        let (a, b) = nodes_both_ways().await;
        let identity = eligible_identity(&a).await.public_key();
        carry(&a, &b, &identity).await.unwrap();
        trade_and_rate(&b, &identity, 10, a_week_ago(), &[5, 5, 4, 5, 5]).await;
        carry(&b, &a, &identity).await.unwrap();

        // A sixth rating on A changes what A attests, so its next
        // attestation is a new one even within the same second.
        trade_and_rate(&a, &identity, 0, 0, &[5]).await;
        let json = export(&a, &identity, &identity).await.unwrap();
        let attestation = attested(&json);
        assert_eq!(attestation.reviews, 6, "A's ratings, not B's on top");
        assert_eq!(attestation.rating_text(), "4.50");
        assert_eq!(attestation.since, 1_696_204_800);
        assert_eq!(
            refused(import_on(&b, &identity, json).await),
            CantDoReason::ReputationAlreadyImported
        );

        trade_and_rate(&b, &identity, 0, 0, &[5]).await;
        assert_eq!(
            refused(carry(&b, &a, &identity).await),
            CantDoReason::ReputationAlreadyImported
        );
        // Each node: its own six ratings and the other's five, each counted once.
        for (node, reviews) in [(&a, 11), (&b, 11)] {
            let user = is_user_present(node.pool(), identity.to_hex())
                .await
                .unwrap();
            assert_eq!(user.total_reviews, reviews);
        }
    }

    /// The way back also works when the identity on B is another key: A
    /// binds the account to that key, B binds the account it seeded to the
    /// identity on A, and A records B's account as the source.
    #[tokio::test]
    async fn the_way_back_works_with_another_identity_on_b() {
        let (a, b) = nodes_both_ways().await;
        let on_a = eligible_identity(&a).await.public_key();
        let on_b = Keys::generate().public_key();
        let json = export(&a, &on_a, &on_b).await.unwrap();
        import_on(&b, &on_b, json).await.unwrap();
        trade_and_rate(&b, &on_b, 10, a_week_ago(), &[5, 5, 4, 5, 5]).await;

        let json = export(&b, &on_b, &on_a).await.unwrap();
        let attestation = attested(&json);
        assert_eq!(attestation.subject, on_b.to_hex());
        assert_eq!(attestation.destination, on_a);
        import_on(&a, &on_a, json).await.unwrap();

        let user = is_user_present(a.pool(), on_a.to_hex()).await.unwrap();
        assert_eq!(user.total_reviews, 10);
        assert_eq!(user.reputation_exported_to, Some(on_b.to_hex()));
        let (subject,): (String,) =
            sqlx::query_as("SELECT subject FROM reputation_imports WHERE identity_pubkey = ?1")
                .bind(on_a.to_hex())
                .fetch_one(a.pool())
                .await
                .unwrap();
        assert_eq!(subject, on_b.to_hex());
        let user = is_user_present(b.pool(), on_b.to_hex()).await.unwrap();
        assert_eq!(user.reputation_exported_to, Some(on_a.to_hex()));
    }

    fn rebind_request(
        identity: &PublicKey,
        destination: &PublicKey,
        rebind: String,
    ) -> (Message, UnwrappedMessage) {
        let msg = Message::new_order(
            None,
            None,
            None,
            Action::ExportReputation,
            Some(Payload::ReputationExportRequest(ReputationExportRequest {
                destination: destination.to_hex(),
                rebind: Some(rebind),
            })),
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

    fn authorisation(signer: &Keys, issuer: &PublicKey, new: &PublicKey) -> String {
        ReputationRebind::build(signer, issuer, new, Timestamp::now(), 600)
            .unwrap()
            .as_json()
    }

    async fn bound_to(ctx: &AppContext, identity: &PublicKey) -> Option<String> {
        is_user_present(ctx.pool(), identity.to_hex())
            .await
            .unwrap()
            .reputation_exported_to
    }

    #[tokio::test]
    async fn a_valid_authorisation_moves_the_binding_once_and_a_replay_changes_nothing() {
        let issuer = Keys::generate();
        let ctx = issuer_node(&issuer).await;
        let identity = eligible_identity(&ctx).await.public_key();
        let (x, y) = (Keys::generate(), Keys::generate());
        export(&ctx, &identity, &x.public_key()).await.unwrap();

        let auth = authorisation(&x, &issuer.public_key(), &y.public_key());
        let (msg, event) = rebind_request(&identity, &y.public_key(), auth.clone());
        export_reputation_action(&ctx, msg, &event).await.unwrap();
        assert_eq!(
            bound_to(&ctx, &identity).await,
            Some(y.public_key().to_hex())
        );

        // Replayed as is: already bound to Y, so it only re-issues.
        let (msg, event) = rebind_request(&identity, &y.public_key(), auth.clone());
        export_reputation_action(&ctx, msg, &event).await.unwrap();
        assert_eq!(
            bound_to(&ctx, &identity).await,
            Some(y.public_key().to_hex())
        );

        // Once the binding moved on to Z, the old authorisation is refused.
        let z = Keys::generate();
        let (msg, event) = rebind_request(
            &identity,
            &z.public_key(),
            authorisation(&y, &issuer.public_key(), &z.public_key()),
        );
        export_reputation_action(&ctx, msg, &event).await.unwrap();
        let (msg, event) = rebind_request(&identity, &y.public_key(), auth);
        assert_eq!(
            refused(export_reputation_action(&ctx, msg, &event).await),
            CantDoReason::InvalidReputationRebind
        );
        assert_eq!(
            bound_to(&ctx, &identity).await,
            Some(z.public_key().to_hex())
        );
    }

    #[tokio::test]
    async fn an_authorisation_signed_by_any_other_key_or_for_anything_else_is_refused() {
        let issuer = Keys::generate();
        let ctx = issuer_node(&issuer).await;
        let identity = eligible_identity(&ctx).await.public_key();
        let (x, y) = (Keys::generate(), Keys::generate());
        export(&ctx, &identity, &x.public_key()).await.unwrap();

        let stranger = Keys::generate();
        let expired = ReputationRebind::build(
            &x,
            &issuer.public_key(),
            &y.public_key(),
            Timestamp::from(Timestamp::now().as_secs() - 7_200),
            600,
        )
        .unwrap()
        .as_json();
        for (why, auth, destination) in [
            (
                "signed by another key",
                authorisation(&stranger, &issuer.public_key(), &y.public_key()),
                y.public_key(),
            ),
            (
                "for another issuer",
                authorisation(&x, &Keys::generate().public_key(), &y.public_key()),
                y.public_key(),
            ),
            (
                "for another identity",
                authorisation(&x, &issuer.public_key(), &stranger.public_key()),
                y.public_key(),
            ),
            ("expired", expired, y.public_key()),
            ("not an event", "{}".to_string(), y.public_key()),
        ] {
            let (msg, event) = rebind_request(&identity, &destination, auth);
            assert_eq!(
                refused(export_reputation_action(&ctx, msg, &event).await),
                CantDoReason::InvalidReputationRebind,
                "{why}"
            );
        }
        assert_eq!(
            bound_to(&ctx, &identity).await,
            Some(x.public_key().to_hex())
        );
    }

    /// The log of an owner's move names the trade key and the authorisation,
    /// never the account nor either binding: together they would tie three
    /// long-lived identities in the logs.
    #[tokio::test]
    async fn the_owners_move_log_names_neither_the_account_nor_either_binding() {
        let issuer = Keys::generate();
        let ctx = issuer_node(&issuer).await;
        let identity = eligible_identity(&ctx).await.public_key();
        let (x, y) = (Keys::generate(), Keys::generate());
        export(&ctx, &identity, &x.public_key()).await.unwrap();
        let auth = authorisation(&x, &issuer.public_key(), &y.public_key());
        let rebind_id = ReputationRebind::parse_json(&auth, Timestamp::now())
            .unwrap()
            .id
            .to_hex();
        let (msg, event) = rebind_request(&identity, &y.public_key(), auth);
        let (result, logs) =
            test_utils::logged_on_this_thread(export_reputation_action(&ctx, msg, &event)).await;
        result.unwrap();

        assert!(logs.contains(&rebind_id), "{logs}");
        assert!(logs.contains(&event.sender.to_hex()), "{logs}");
        for key in [identity, x.public_key(), y.public_key()] {
            assert!(!logs.contains(&key.to_hex()), "{logs}");
        }
    }

    /// An authorisation moves a binding once (§5.2): replayed after the
    /// binding came back to its signer, it is refused and the binding stays,
    /// while a fresh one from the signer still moves it.
    #[tokio::test]
    async fn an_authorisation_moves_a_binding_once_even_after_it_comes_back() {
        let issuer = Keys::generate();
        let ctx = issuer_node(&issuer).await;
        let identity = eligible_identity(&ctx).await.public_key();
        let (x, y) = (Keys::generate(), Keys::generate());
        export(&ctx, &identity, &x.public_key()).await.unwrap();
        let move_to = |signer: &Keys, to: &Keys, auth: Option<String>| {
            let auth = auth
                .unwrap_or_else(|| authorisation(signer, &issuer.public_key(), &to.public_key()));
            rebind_request(&identity, &to.public_key(), auth)
        };

        let x_to_y = authorisation(&x, &issuer.public_key(), &y.public_key());
        let (msg, event) = move_to(&x, &y, Some(x_to_y.clone()));
        export_reputation_action(&ctx, msg, &event).await.unwrap();
        let (msg, event) = move_to(&y, &x, None);
        export_reputation_action(&ctx, msg, &event).await.unwrap();
        assert_eq!(
            bound_to(&ctx, &identity).await,
            Some(x.public_key().to_hex())
        );

        let (msg, event) = move_to(&x, &y, Some(x_to_y));
        assert_eq!(
            refused(export_reputation_action(&ctx, msg, &event).await),
            CantDoReason::InvalidReputationRebind
        );
        assert_eq!(
            bound_to(&ctx, &identity).await,
            Some(x.public_key().to_hex())
        );

        // A new authorisation for the same move: another lifetime gives it
        // another id even within the same second.
        let fresh = ReputationRebind::build(
            &x,
            &issuer.public_key(),
            &y.public_key(),
            Timestamp::now(),
            900,
        )
        .unwrap()
        .as_json();
        let (msg, event) = move_to(&x, &y, Some(fresh));
        export_reputation_action(&ctx, msg, &event).await.unwrap();
        assert_eq!(
            bound_to(&ctx, &identity).await,
            Some(y.public_key().to_hex())
        );
    }

    /// A database failure while the account is read for a move is an
    /// operational failure, not a verdict on the account.
    #[tokio::test]
    async fn a_database_failure_while_rebinding_is_not_reported_as_not_eligible() {
        let issuer = Keys::generate();
        let ctx = issuer_node(&issuer).await;
        let identity = eligible_identity(&ctx).await.public_key();
        let (x, y) = (Keys::generate(), Keys::generate());
        export(&ctx, &identity, &x.public_key()).await.unwrap();
        sqlx::query("ALTER TABLE users RENAME TO users_gone")
            .execute(ctx.pool())
            .await
            .unwrap();
        let result = apply_rebind(
            &ctx,
            &issuer,
            &identity.to_hex(),
            &y.public_key(),
            &authorisation(&x, &issuer.public_key(), &y.public_key()),
            Timestamp::now(),
            &Keys::generate().public_key(),
        )
        .await;
        assert!(
            matches!(
                result,
                Err(MostroInternalErr(ServiceError::DbAccessError(_)))
            ),
            "{result:?}"
        );
    }

    /// The compare-and-set moves one account, and only from the value the
    /// caller read: a stale `from` moves nothing, another account bound to
    /// the same identity stays where it is, and an unbound account moves from
    /// `None`.
    #[tokio::test]
    async fn moving_a_binding_touches_only_the_named_account_from_the_expected_value() {
        let issuer = Keys::generate();
        let ctx = issuer_node(&issuer).await;
        let (a, b, c) = (
            eligible_identity(&ctx).await.public_key(),
            eligible_identity(&ctx).await.public_key(),
            eligible_identity(&ctx).await.public_key(),
        );
        let (x, y) = (Keys::generate().public_key(), Keys::generate().public_key());
        for account in [&a, &b] {
            export(&ctx, account, &x).await.unwrap();
        }
        let (x, y) = (x.to_hex(), y.to_hex());

        let stale = move_reputation_binding(ctx.pool(), &a.to_hex(), Some(&y), &y, 1).await;
        assert!(!stale.unwrap(), "a stale expected value moves nothing");
        assert_eq!(bound_to(&ctx, &a).await, Some(x.clone()));

        let moved = move_reputation_binding(ctx.pool(), &a.to_hex(), Some(&x), &y, 1).await;
        assert!(moved.unwrap());
        assert_eq!(bound_to(&ctx, &a).await, Some(y.clone()));
        assert_eq!(bound_to(&ctx, &b).await, Some(x), "the other account stays");

        let first = move_reputation_binding(ctx.pool(), &c.to_hex(), None, &y, 1).await;
        assert!(first.unwrap(), "an unbound account moves from None");
        assert_eq!(bound_to(&ctx, &c).await, Some(y));
    }

    /// A move that does not apply records nothing: the same authorisation
    /// still moves the binding once the expected value is right.
    #[tokio::test]
    async fn a_move_that_does_not_apply_does_not_use_up_the_authorisation() {
        let issuer = Keys::generate();
        let ctx = issuer_node(&issuer).await;
        let a = eligible_identity(&ctx).await.public_key();
        let (x, y) = (Keys::generate().public_key(), Keys::generate().public_key());
        export(&ctx, &a, &x).await.unwrap();
        let (a, x, y) = (a.to_hex(), x.to_hex(), y.to_hex());
        let id = "ab".repeat(32);
        let valid_until = i64::MAX;

        let stale = move_reputation_binding_once(ctx.pool(), &a, &y, &y, 1, &id, valid_until);
        assert!(!stale.await.unwrap());
        let moved = move_reputation_binding_once(ctx.pool(), &a, &x, &y, 1, &id, valid_until);
        assert!(moved.await.unwrap());
        let again = move_reputation_binding_once(ctx.pool(), &a, &y, &x, 1, &id, valid_until);
        assert!(!again.await.unwrap(), "an authorisation moves once");
        assert_eq!(
            is_user_present(ctx.pool(), a)
                .await
                .unwrap()
                .reputation_exported_to,
            Some(y)
        );
    }

    /// Two moves of one account at once, each with a valid authorisation
    /// from the bound identity: one applies, the other is refused, the
    /// binding is the winner's, the day of the move is recorded and the
    /// account's ratings are untouched.
    #[tokio::test]
    async fn two_owner_moves_at_once_apply_exactly_one() {
        let issuer = Keys::generate();
        let ctx = issuer_node(&issuer).await;
        let identity = eligible_identity(&ctx).await.public_key();
        let (x, y, z) = (Keys::generate(), Keys::generate(), Keys::generate());
        export(&ctx, &identity, &x.public_key()).await.unwrap();
        sqlx::query("UPDATE users SET reputation_exported_at = 0 WHERE pubkey = ?1")
            .bind(identity.to_hex())
            .execute(ctx.pool())
            .await
            .unwrap();
        let before = is_user_present(ctx.pool(), identity.to_hex())
            .await
            .unwrap();
        let today = || day_truncate(Timestamp::now().as_secs() as i64) as i64;
        let first_day = today();

        let (m1, e1) = rebind_request(
            &identity,
            &y.public_key(),
            authorisation(&x, &issuer.public_key(), &y.public_key()),
        );
        let (m2, e2) = rebind_request(
            &identity,
            &z.public_key(),
            authorisation(&x, &issuer.public_key(), &z.public_key()),
        );
        let (r1, r2) = tokio::join!(
            export_reputation_action(&ctx, m1, &e1),
            export_reputation_action(&ctx, m2, &e2)
        );
        let last_day = today();
        assert_eq!([r1.is_ok(), r2.is_ok()].iter().filter(|ok| **ok).count(), 1);
        let (winner, lost) = if r1.is_ok() { (&y, r2) } else { (&z, r1) };
        assert_eq!(refused(lost), CantDoReason::InvalidReputationRebind);
        let after = is_user_present(ctx.pool(), identity.to_hex())
            .await
            .unwrap();
        assert_eq!(
            after.reputation_exported_to,
            Some(winner.public_key().to_hex())
        );
        let day = after.reputation_exported_at.unwrap();
        assert!(day == first_day || day == last_day, "{day}");
        assert_eq!(
            (
                after.total_reviews,
                after.total_rating,
                after.native_rating_sum
            ),
            (
                before.total_reviews,
                before.total_rating,
                before.native_rating_sum
            )
        );
    }
}
