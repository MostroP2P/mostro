//! Issuing reputation attestations (docs/REPUTATION_PORTABILITY.md §5.2,
//! phase 4): what this node attests, and to whom.

use crate::app::context::AppContext;
use crate::config::types::ReputationExportSettings;
use crate::db::{bind_reputation_export, completed_trades_for_identity, find_user_by_pubkey};
use crate::util::enqueue_order_msg;
use mostro_core::prelude::*;
use mostro_core::reputation::MIN_ATTESTED_SINCE;
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
/// refused. A failure to sign binds nothing. The client asked the user to
/// confirm the destination before sending the request, which the source
/// identity signs.
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
        return Err(MostroCantDo(CantDoReason::ReputationBoundToOtherIdentity));
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
    use crate::app::context::test_utils::{test_settings, TestContextBuilder};
    use crate::app::reputation::import::import_reputation_action;
    use crate::config::types::{ReputationImportSettings, ReputationIssuer};
    use crate::db::{add_new_user, is_user_present};
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

    /// An identity with 10 completed trades and native ratings 5, 4, 5, 5, 3
    /// on `ctx`'s node, first trade taken on 2023-10-02.
    async fn eligible_identity(ctx: &AppContext) -> Keys {
        identity_with(ctx, 10, &[5, 4, 5, 5, 3]).await
    }

    /// An identity with `trades` completed trades and the given native
    /// ratings on `ctx`'s node, first trade taken on 2023-10-02.
    async fn identity_with(ctx: &AppContext, trades: i64, ratings: &[u8]) -> Keys {
        let identity = Keys::generate();
        let hex = identity.public_key().to_hex();
        let mut user = User::new(hex.clone(), 0, 0, 0, 0, 1);
        for rating in ratings {
            user.update_rating(*rating);
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
        for i in 0..trades {
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

    thread_local! {
        static LOGGED: std::cell::RefCell<Option<Vec<u8>>> = const { std::cell::RefCell::new(None) };
    }

    /// What this thread logs while `run` runs. A subscriber scoped to the
    /// test with `set_default` would miss lines: while it is the only one
    /// registered, a test on another thread that reaches the same log line
    /// first caches it as disabled. A global one, installed once, is the
    /// subscriber every thread sees; it keeps only the lines of a thread
    /// that is capturing.
    async fn logged_on_this_thread<F: std::future::Future>(run: F) -> (F::Output, String) {
        struct ThreadWriter;
        impl std::io::Write for ThreadWriter {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                LOGGED.with(|logged| {
                    if let Some(logged) = logged.borrow_mut().as_mut() {
                        logged.extend_from_slice(buf);
                    }
                });
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        static INSTALL: std::sync::Once = std::sync::Once::new();
        INSTALL.call_once(|| {
            let subscriber = tracing_subscriber::fmt()
                .with_writer(|| ThreadWriter)
                .with_ansi(false)
                .finish();
            tracing::subscriber::set_global_default(subscriber).unwrap();
        });
        LOGGED.with(|logged| *logged.borrow_mut() = Some(Vec::new()));
        let output = run.await;
        let logged = LOGGED.with(|logged| logged.borrow_mut().take().unwrap_or_default());
        (output, String::from_utf8(logged).unwrap())
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
        let (json, logs) = logged_on_this_thread(export(&ctx, &identity, &destination)).await;
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
}
