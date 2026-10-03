//! Issuing reputation attestations (docs/REPUTATION_PORTABILITY.md §5.2,
//! phase 4): what this node attests, and to whom.

use crate::app::context::AppContext;
use crate::config::types::ReputationExportSettings;
use crate::db::{bind_reputation_export, completed_trades_for_identity, is_user_present};
use crate::util::enqueue_order_msg;
use mostro_core::prelude::*;
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
    /// UTC day start of the first completed trade here.
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
/// the identity the transport proved, never the payload. The account is bound
/// to the destination with a compare-and-set before anything is signed: a
/// request for the identity already bound is how a lost, expired or fresher
/// attestation is obtained; one for another identity is refused. The client
/// asked the user to confirm the destination before sending the request,
/// which the source identity signs.
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
    let user = is_user_present(ctx.pool(), identity.clone())
        .await
        .map_err(|_| not_eligible())?;
    let native = native_reputation(ctx.pool(), &user).await?;
    if !native.is_eligible(&user) {
        return Err(not_eligible());
    }
    let since = native.since.ok_or_else(not_eligible)?;

    let now = Timestamp::now();
    let today = day_truncate(now.as_secs() as i64) as i64;
    let bound = bind_reputation_export(ctx.pool(), &identity, &destination.to_hex(), today).await?;
    if !bound {
        return Err(MostroCantDo(CantDoReason::ReputationBoundToOtherIdentity));
    }

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
    tracing::info!(
        "reputation: exported {} ratings of {} to {} (attestation {})",
        native.reviews,
        identity,
        destination.to_hex(),
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
    let (completed_trades, first_trade) = completed_trades_for_identity(pool, &user.pubkey).await?;
    Ok(NativeReputation {
        reviews,
        rating,
        completed_trades,
        since: first_trade.filter(|t| *t > 0).map(day_truncate),
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
    use crate::app::context::test_utils::{test_settings, TestContextBuilder};
    use crate::app::reputation::import::import_reputation_action;
    use crate::config::types::{ReputationImportSettings, ReputationIssuer};
    use crate::db::add_new_user;
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
        let _ = crate::config::MOSTRO_CONFIG.set(test_settings());
        let mut settings = test_settings();
        settings.reputation_export = Some(ReputationExportSettings {
            enabled: true,
            issuer_keys: Some(issuer.clone()),
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

    /// An identity with 10 completed trades and native ratings 5, 4, 5, 5, 3
    /// on `ctx`'s node, first trade taken on 2023-10-02.
    async fn eligible_identity(ctx: &AppContext) -> Keys {
        let identity = Keys::generate();
        let hex = identity.public_key().to_hex();
        let mut user = User::new(hex.clone(), 0, 0, 0, 0, 1);
        for rating in [5, 4, 5, 5, 3] {
            user.update_rating(rating);
        }
        add_new_user(ctx.pool(), user.clone()).await.unwrap();
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
        for i in 0..10 {
            Order {
                id: Uuid::new_v4(),
                status: Status::Success.to_string(),
                kind: mostro_core::order::Kind::Sell.to_string(),
                fiat_code: "USD".to_string(),
                creator_pubkey: Keys::generate().public_key().to_string(),
                amount: 10_000,
                created_at: 1_696_200_000,
                taken_at: 1_696_250_000 + i,
                master_seller_pubkey: Some(hex.clone()),
                ..Default::default()
            }
            .create(ctx.pool())
            .await
            .unwrap();
        }
        identity
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
        let json = export(&ctx, &identity, &destination).await.unwrap();

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
        assert_eq!(user.reputation_exported_at.unwrap() % 86_400, 0);
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
        for bad in ["", "AB", &destination.to_hex().to_uppercase()] {
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
    /// process. B ends up with A's native figures, and cannot import them
    /// twice.
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

        let (msg, event) = import(json);
        assert_eq!(
            refused(import_reputation_action(&b, msg, &event, &Keys::generate()).await),
            CantDoReason::ReputationAlreadyImported
        );
    }
}
