//! The health-event job: collect the facts, evaluate the checks, publish
//! (protocol `health_event.md`, "Publishing").
//!
//! The job evaluates every [`EVALUATION_SECS`] and on every maintenance
//! change. It publishes when `interval` has elapsed since the last
//! publication, or at once when `status`, a check state or maintenance
//! changed, but never twice within [`MIN_REPUBLISH_SECS`].

use crate::app::context::AppContext;
use crate::app::dev_fee::dev_fee_payments_enabled;
use crate::app::health::check::{
    database_check, derive_status, payouts_check, relays_check, CheckName, CheckState,
    CheckTracker, HealthStatus, PayoutFacts,
};
use crate::app::health::content::{
    nearest_htlc_expiry_blocks, ChannelFacts, DevFeeContent, HealthContent, LndContent,
    MaintenanceContent, PayoutsContent, RelaysContent, CONTENT_VERSION, MAX_CONTENT_BYTES,
};
use crate::app::health::event::{
    build_health_event, load_persisted, next_stamp, save_relay_outage, save_stamp, HealthReport,
    LndTags, PersistedHealth,
};
use crate::app::health::lnd_sync::LndSyncSnapshot;
use crate::app::health::stats::{dev_fee_failures_1h, last_payout_success_at, WINDOW_SECS};
use crate::config::types::{DEFAULT_HEALTH_INTERVAL, HEALTH_INTERVAL_RANGE};
use crate::lightning::LndConnector;
use crate::LN_STATUS;
use chrono::Utc;
use mostro_core::error::MostroError::{self, MostroInternalErr};
use mostro_core::error::ServiceError;
use mostro_core::prelude::Status;
use nostr_sdk::prelude::PublicKey;
use sqlx::SqlitePool;
use std::time::{Duration, Instant};
use tracing::{error, info, warn};

/// How often the checks are evaluated, seconds.
pub const EVALUATION_SECS: u64 = 15;
/// Minimum time between two publications, seconds.
pub const MIN_REPUBLISH_SECS: u64 = 10;
/// Bound on the database probe (`database` check), seconds.
const DATABASE_PROBE_TIMEOUT: Duration = Duration::from_secs(5);
/// Bound on `ListChannels`.
const LND_CALL_TIMEOUT: Duration = Duration::from_secs(10);

/// `[health].interval`, clamped to the protocol's range.
pub fn resolve_interval(configured: u64) -> u64 {
    if HEALTH_INTERVAL_RANGE.contains(&configured) {
        return configured;
    }
    let clamped = configured.clamp(*HEALTH_INTERVAL_RANGE.start(), *HEALTH_INTERVAL_RANGE.end());
    error!(
        "[health].interval = {configured} is outside {}..={}; using {clamped}",
        HEALTH_INTERVAL_RANGE.start(),
        HEALTH_INTERVAL_RANGE.end()
    );
    clamped
}

/// `[health].watchdog_pubkey` (hex or npub). Empty means no recipient; an
/// invalid key is logged and also means no recipient, so the public tags are
/// still published.
pub fn resolve_recipient(configured: &str) -> Option<PublicKey> {
    let configured = configured.trim();
    if configured.is_empty() {
        return None;
    }
    match PublicKey::parse(configured) {
        Ok(key) => Some(key),
        Err(e) => {
            error!(
                "[health].watchdog_pubkey is not a valid public key ({e}); \
                 publishing the health event without encrypted content"
            );
            None
        }
    }
}

/// What the database says about buyer payouts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PayoutRows {
    pub settled_unpaid: u64,
    pub oldest_settled_unpaid_age_secs: Option<i64>,
    pub awaiting_buyer_invoice: u64,
}

fn db_error(e: sqlx::Error) -> MostroError {
    MostroInternalErr(ServiceError::DbAccessError(e.to_string()))
}

/// Orders in `settled-hold-invoice`, split into those the daemon is still
/// trying to pay ("settled unpaid") and those waiting on the buyer for a new
/// invoice (attempts used up, no payout in flight).
pub async fn payout_rows(
    pool: &SqlitePool,
    payment_attempts: i64,
    now: i64,
) -> Result<PayoutRows, MostroError> {
    // "Waiting on the buyer": attempts used up and no payout in flight, the
    // state in which `check_failure_retries` has sent `add-invoice`.
    const AWAITING: &str = "SELECT COUNT(*) FROM orders WHERE status = ?1 \
         AND failed_payment = 1 AND payout_payment_hash IS NULL AND payment_attempts >= ?2";
    const UNPAID: &str = "SELECT COUNT(*), MIN(COALESCE(hold_settled_at, invoice_held_at)) \
         FROM orders WHERE status = ?1 \
         AND NOT (failed_payment = 1 AND payout_payment_hash IS NULL AND payment_attempts >= ?2)";
    let settled = Status::SettledHoldInvoice.to_string();
    let awaiting: i64 = sqlx::query_scalar(AWAITING)
        .bind(&settled)
        .bind(payment_attempts)
        .fetch_one(pool)
        .await
        .map_err(db_error)?;
    let (unpaid, oldest): (i64, Option<i64>) = sqlx::query_as(UNPAID)
        .bind(&settled)
        .bind(payment_attempts)
        .fetch_one(pool)
        .await
        .map_err(db_error)?;
    Ok(PayoutRows {
        settled_unpaid: unpaid.max(0) as u64,
        oldest_settled_unpaid_age_secs: oldest
            .filter(|_| unpaid > 0)
            .map(|at| now.saturating_sub(at).max(0)),
        awaiting_buyer_invoice: awaiting.max(0) as u64,
    })
}

/// Completed orders whose dev fee is not paid yet.
pub async fn dev_fee_pending(pool: &SqlitePool) -> Result<u64, MostroError> {
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM orders \
         WHERE (status = 'settled-hold-invoice' OR status = 'success') \
           AND dev_fee > 0 AND dev_fee_paid = 0",
    )
    .fetch_one(pool)
    .await
    .map_err(db_error)?;
    Ok(count.max(0) as u64)
}

/// The `database` probe: a trivial query within [`DATABASE_PROBE_TIMEOUT`].
async fn database_answers(pool: &SqlitePool) -> bool {
    matches!(
        tokio::time::timeout(
            DATABASE_PROBE_TIMEOUT,
            sqlx::query("SELECT 1").execute(pool)
        )
        .await,
        Ok(Ok(_))
    )
}

/// The `lnd` content and the LND tags from the sync monitor's snapshot and
/// `ListChannels`. A stale snapshot or a failed `GetInfo` gives an
/// unreachable LND and no tags.
pub fn lnd_view(
    snapshot: &LndSyncSnapshot,
    channels: Option<ChannelFacts>,
    fallback_network: Option<&str>,
    now: i64,
    fail_after: u64,
) -> (LndContent, Option<LndTags>) {
    let reachable = snapshot.sync_check(now, fail_after) != CheckState::Unknown;
    let Some(last) = snapshot.last.as_ref().filter(|_| reachable) else {
        return (LndContent::unreachable(), None);
    };
    let content = LndContent {
        reachable: true,
        synced_to_chain: Some(last.synced_to_chain),
        unsynced_since: snapshot.unsynced_since.filter(|_| !last.synced_to_chain),
        block_height: Some(last.block_height),
        best_header_timestamp: Some(last.best_header_timestamp),
        peers: Some(last.peers),
        channels_active: Some(last.channels_active),
        channels_inactive: Some(last.channels_inactive),
        channels_pending: Some(last.channels_pending),
        outbound_liquidity_sats: channels.map(|c| c.outbound_liquidity_sats),
        inbound_liquidity_sats: channels.map(|c| c.inbound_liquidity_sats),
        pending_htlcs: channels.map(|c| c.pending_htlcs),
        nearest_htlc_expiry_blocks: channels.and_then(|c| {
            nearest_htlc_expiry_blocks(c.nearest_htlc_expiration_height, last.block_height)
        }),
    };
    let network = last
        .network
        .clone()
        .or_else(|| fallback_network.map(str::to_string));
    let tags = network.map(|network| LndTags {
        block_height: last.block_height,
        synced_to_chain: last.synced_to_chain,
        network,
    });
    (content, tags)
}

/// What a change of triggers an immediate republish: `status`, every
/// check state, and the maintenance flag.
type Signature = (HealthStatus, Vec<(CheckName, CheckState)>, bool);

/// One evaluation: the public report and the private payload.
#[derive(Clone, Debug)]
pub struct Evaluation {
    pub report: HealthReport,
    pub content: HealthContent,
}

impl Evaluation {
    fn signature(&self) -> Signature {
        (
            self.report.status,
            self.report
                .checks
                .iter()
                .map(|(name, state, _)| (*name, *state))
                .collect(),
            self.report.maintenance,
        )
    }
}

/// State the job keeps between evaluations.
pub struct Publisher {
    interval: u64,
    recipient: Option<PublicKey>,
    tracker: CheckTracker,
    persisted: PersistedHealth,
    had_relay_connection: bool,
    ln_client: Option<LndConnector>,
    last_publish: Option<Instant>,
    last_signature: Option<Signature>,
}

impl Publisher {
    pub async fn new(ctx: &AppContext) -> Self {
        let persisted = match load_persisted(ctx.pool()).await {
            Ok(persisted) => persisted,
            Err(e) => {
                error!("Could not load the health event state, starting from scratch: {e}");
                PersistedHealth::default()
            }
        };
        Self {
            interval: resolve_interval(ctx.settings().health.interval),
            recipient: resolve_recipient(&ctx.settings().health.watchdog_pubkey),
            tracker: CheckTracker::default(),
            persisted,
            had_relay_connection: false,
            ln_client: None,
            last_publish: None,
            last_signature: None,
        }
    }

    async fn channel_facts(&mut self) -> Option<ChannelFacts> {
        if self.ln_client.is_none() {
            self.ln_client = LndConnector::new().await.ok();
        }
        let client = self.ln_client.as_mut()?;
        match tokio::time::timeout(LND_CALL_TIMEOUT, client.channel_facts()).await {
            Ok(Ok(facts)) => Some(facts),
            Ok(Err(e)) => {
                warn!("Health event: ListChannels failed: {e}");
                self.ln_client = None;
                None
            }
            Err(_) => {
                warn!("Health event: ListChannels did not answer in time");
                self.ln_client = None;
                None
            }
        }
    }

    /// Track the relay outage and persist it when it changes.
    async fn observe_relays(&mut self, pool: &SqlitePool, connected: usize, now: i64) {
        if let Some(outage) =
            self.persisted
                .relay_outage
                .observe(self.had_relay_connection, connected, now)
        {
            if outage.ended_at.is_none() {
                warn!("Lost every relay connection");
            } else {
                info!("A relay is connected again after an outage");
            }
            if let Err(e) = save_relay_outage(pool, outage).await {
                error!("Could not persist the relay outage: {e}");
            }
            self.persisted.relay_outage = outage;
        }
        if connected > 0 {
            self.had_relay_connection = true;
        }
    }

    /// Collect every fact and evaluate the checks at `now`.
    pub async fn evaluate(&mut self, ctx: &AppContext, now: i64) -> Evaluation {
        let pool = ctx.pool();
        let settings = ctx.settings();
        let lightning = !settings.cashu.as_ref().is_some_and(|cashu| cashu.enabled);

        // Relays.
        let relays = ctx.nostr_client().relays().await;
        let configured = relays.len();
        let connected = relays
            .values()
            .filter(|relay| relay.status().is_connected())
            .count();
        self.observe_relays(pool, connected, now).await;

        // Database.
        let database_ok = database_answers(pool).await;

        // Maintenance.
        let maintenance_enabled = ctx.maintenance().is_enabled();
        let maintenance_reason = match ctx.maintenance().reason(pool).await {
            Ok(reason) => reason,
            Err(e) => {
                warn!("Health event: could not read the maintenance reason: {e}");
                None
            }
        };

        let mut checks: Vec<(CheckName, CheckState)> = Vec::with_capacity(4);
        let mut lnd_content = None;
        let mut payouts_content = None;
        let mut dev_fee_content = None;
        let mut lnd_tags = None;

        if lightning {
            let fail_after = settings.health.lnd_unsynced_fail_after;
            let snapshot = ctx.lnd_sync().snapshot();
            let channels = if snapshot.sync_check(now, fail_after) == CheckState::Unknown {
                None
            } else {
                self.channel_facts().await
            };
            let networks = LN_STATUS.get().map(|status| status.networks.clone());
            let fallback = networks
                .as_ref()
                .and_then(|n| n.first().map(String::as_str));
            let (content, tags) = lnd_view(&snapshot, channels, fallback, now, fail_after);
            lnd_content = Some(content);
            lnd_tags = tags;
            checks.push((CheckName::LndSync, snapshot.sync_check(now, fail_after)));

            let window = ctx.health_stats().payout_window(now);
            let rows = payout_rows(pool, i64::from(settings.lightning.payment_attempts), now)
                .await
                .inspect_err(|e| warn!("Health event: payout query failed: {e}"))
                .ok();
            let last_success = last_payout_success_at(pool)
                .await
                .inspect_err(|e| warn!("Health event: last payout query failed: {e}"))
                .ok();
            let facts = match (rows, last_success) {
                (Some(rows), Some(last_success_at)) => Some(PayoutFacts {
                    succeeded_1h: window.succeeded,
                    failed_1h: window.failed,
                    last_success_at,
                    oldest_settled_unpaid_age_secs: rows.oldest_settled_unpaid_age_secs,
                }),
                _ => None,
            };
            checks.push((CheckName::Payouts, payouts_check(facts.as_ref(), now)));
            payouts_content = Some(PayoutsContent {
                window_secs: WINDOW_SECS,
                payouts_succeeded_1h: window.succeeded,
                payouts_failed_1h: window.failed,
                failure_reasons_1h: window.failure_reasons,
                last_payout_success_at: last_success.flatten(),
                settled_unpaid_orders: rows.map(|r| r.settled_unpaid),
                oldest_settled_unpaid_order_age_secs: rows
                    .and_then(|r| r.oldest_settled_unpaid_age_secs),
                awaiting_buyer_invoice_orders: rows.map(|r| r.awaiting_buyer_invoice),
            });

            let pays_dev_fee = settings.mostro.dev_fee_percentage > 0.0
                && networks.as_deref().is_some_and(dev_fee_payments_enabled);
            if pays_dev_fee {
                dev_fee_content = Some(DevFeeContent {
                    pending: dev_fee_pending(pool)
                        .await
                        .inspect_err(|e| warn!("Health event: dev fee query failed: {e}"))
                        .ok(),
                    failed_1h: dev_fee_failures_1h(now),
                });
            }
        }
        if let Some(cashu) = settings.cashu.as_ref().filter(|cashu| cashu.enabled) {
            checks.push((
                CheckName::CashuMint,
                crate::app::health::cashu_mint::cashu_mint_check(pool, cashu).await,
            ));
        }
        checks.push((CheckName::Relays, relays_check(configured, connected)));
        checks.push((CheckName::Database, database_check(database_ok)));

        let status = derive_status(&checks, maintenance_enabled);
        let report = HealthReport {
            status,
            checks: checks
                .iter()
                .map(|(name, state)| (*name, *state, self.tracker.update(*name, *state, now)))
                .collect(),
            escrow_mode: if lightning { "lightning" } else { "cashu" },
            lnd: lnd_tags,
            maintenance: maintenance_enabled,
            instance_name: settings.mostro.name.clone(),
            interval: self.interval,
        };
        let content = HealthContent {
            v: CONTENT_VERSION,
            generated_at: now,
            lnd: lnd_content,
            payouts: payouts_content,
            dev_fee: dev_fee_content,
            relays: RelaysContent {
                configured: configured as u64,
                connected: connected as u64,
                last_outage_started_at: self.persisted.relay_outage.started_at,
                last_outage_ended_at: self.persisted.relay_outage.ended_at,
            },
            maintenance: MaintenanceContent {
                enabled: maintenance_enabled,
                reason: maintenance_reason,
            },
        };
        Evaluation { report, content }
    }

    /// Whether `evaluation` must be published now.
    fn should_publish(&self, evaluation: &Evaluation) -> bool {
        let Some(last) = self.last_publish else {
            return true;
        };
        let elapsed = last.elapsed();
        if elapsed >= Duration::from_secs(self.interval) {
            return true;
        }
        let changed = self.last_signature.as_ref() != Some(&evaluation.signature());
        changed && elapsed >= Duration::from_secs(MIN_REPUBLISH_SECS)
    }

    /// Stamp, persist, sign and send.
    async fn publish(&mut self, ctx: &AppContext, evaluation: &Evaluation, now: i64) {
        let (stamp, ahead) = next_stamp(self.persisted.last_stamp, now);
        if ahead {
            error!(
                "Health event created_at {} is more than 300s ahead of the clock ({now}): \
                 the host clock moved backwards. Readers reject the event until it catches up",
                stamp.created_at
            );
        }
        if let Err(e) = save_stamp(ctx.pool(), stamp).await {
            error!("Could not persist the health event seq/created_at: {e}");
        }
        self.persisted.last_stamp = Some(stamp);

        let mut recipient = self.recipient;
        let payload = match serde_json::to_string(&evaluation.content) {
            Ok(payload) if payload.len() <= MAX_CONTENT_BYTES => payload,
            Ok(payload) => {
                error!(
                    "Health event payload is {} bytes, over the {MAX_CONTENT_BYTES}-byte limit; \
                     publishing the public tags only",
                    payload.len()
                );
                recipient = None;
                String::new()
            }
            Err(e) => {
                error!("Could not serialize the health event payload: {e}");
                recipient = None;
                String::new()
            }
        };
        let event = match build_health_event(
            ctx.keys(),
            &evaluation.report,
            stamp,
            &payload,
            recipient.as_ref(),
        ) {
            Ok(event) => event,
            Err(e) => return error!("Could not build the health event: {e}"),
        };
        match ctx.nostr_client().send_event(&event).await {
            Ok(_) => info!(
                "Published health event: status {}, seq {}",
                evaluation.report.status.as_str(),
                stamp.seq
            ),
            Err(e) => warn!("Could not publish the health event: {e}"),
        }
        self.last_publish = Some(Instant::now());
        self.last_signature = Some(evaluation.signature());
    }

    /// Evaluate and publish if due.
    pub async fn tick(&mut self, ctx: &AppContext) {
        let now = Utc::now().timestamp();
        let evaluation = self.evaluate(ctx, now).await;
        if !has_a_relay(&evaluation) {
            // Nobody to publish to. Publishing anyway would only report a
            // relay outage that is already over by the time a connection
            // carries it (at boot, the first evaluation runs before the
            // relays connect). Readers see the event go stale instead, and
            // the outage reaches the watchdog through `relays.last_outage_*`.
            return;
        }
        if self.should_publish(&evaluation) {
            self.publish(ctx, &evaluation, now).await;
        }
    }
}

/// Whether at least one relay was connected when `evaluation` was made.
fn has_a_relay(evaluation: &Evaluation) -> bool {
    evaluation
        .report
        .checks
        .iter()
        .any(|(name, state, _)| *name == CheckName::Relays && *state != CheckState::Fail)
}

/// The job loop. Returns at once when `[health].enabled = false`.
pub async fn run_health_publisher(ctx: AppContext) {
    if !ctx.settings().health.enabled {
        return info!("Health event disabled ([health].enabled = false)");
    }
    let mut publisher = Publisher::new(&ctx).await;
    if publisher.interval != DEFAULT_HEALTH_INTERVAL {
        info!("Health event every {}s", publisher.interval);
    }
    loop {
        publisher.tick(&ctx).await;
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(EVALUATION_SECS)) => {}
            _ = ctx.maintenance().health_changed() => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::context::test_utils::{test_settings, TestContextBuilder};
    use crate::app::health::lnd_sync::{LndObservation, LndSyncState};
    use std::sync::Arc;

    const NOW: i64 = 1_792_000_000;

    async fn pool() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        pool
    }

    fn observation(synced: bool, network: Option<&str>) -> LndObservation {
        LndObservation {
            synced_to_chain: synced,
            block_height: 971_349,
            best_header_timestamp: NOW - 30,
            network: network.map(str::to_string),
            peers: 19,
            channels_active: 8,
            channels_inactive: 1,
            channels_pending: 0,
        }
    }

    #[test]
    fn interval_is_clamped_to_the_protocol_range() {
        assert_eq!(resolve_interval(60), 60);
        assert_eq!(resolve_interval(30), 30);
        assert_eq!(resolve_interval(3_600), 3_600);
        assert_eq!(resolve_interval(5), 30);
        assert_eq!(resolve_interval(86_400), 3_600);
    }

    #[test]
    fn recipient_accepts_hex_and_npub_and_ignores_garbage() {
        use nostr_sdk::prelude::{Keys, ToBech32};
        let key = Keys::generate().public_key();
        assert_eq!(resolve_recipient(""), None);
        assert_eq!(resolve_recipient("  "), None);
        assert_eq!(resolve_recipient(&key.to_hex()), Some(key));
        assert_eq!(resolve_recipient(&key.to_bech32().unwrap()), Some(key));
        assert_eq!(resolve_recipient("not-a-key"), None);
    }

    #[test]
    fn lnd_view_of_a_synced_node() {
        let state = LndSyncState::new();
        state.observe(observation(true, Some("mainnet")), NOW);
        let channels = ChannelFacts {
            outbound_liquidity_sats: 52_341_870,
            inbound_liquidity_sats: 13_874_104,
            pending_htlcs: 2,
            nearest_htlc_expiration_height: Some(971_640),
        };
        let (content, tags) = lnd_view(&state.snapshot(), Some(channels), None, NOW, 600);
        assert!(content.reachable);
        assert_eq!(content.synced_to_chain, Some(true));
        assert_eq!(content.unsynced_since, None, "null while synced");
        assert_eq!(content.nearest_htlc_expiry_blocks, Some(291));
        assert_eq!(content.outbound_liquidity_sats, Some(52_341_870));
        assert_eq!(
            tags,
            Some(LndTags {
                block_height: 971_349,
                synced_to_chain: true,
                network: "mainnet".to_string()
            })
        );
    }

    #[test]
    fn lnd_view_of_an_unsynced_node_reports_since_when() {
        let state = LndSyncState::new();
        state.observe(observation(false, Some("mainnet")), NOW - 900);
        state.observe(observation(false, Some("mainnet")), NOW);
        let (content, _) = lnd_view(&state.snapshot(), None, None, NOW, 600);
        assert_eq!(content.synced_to_chain, Some(false));
        assert_eq!(content.unsynced_since, Some(NOW - 900));
        // ListChannels failed: those fields are unknown, the rest is known.
        assert_eq!(content.outbound_liquidity_sats, None);
        assert_eq!(content.pending_htlcs, None);
        assert_eq!(content.peers, Some(19));
    }

    #[test]
    fn lnd_view_of_an_unreachable_node_has_no_tags() {
        let state = LndSyncState::new();
        state.observe(observation(true, Some("mainnet")), NOW - 30);
        state.observe_unreachable(NOW);
        let (content, tags) = lnd_view(&state.snapshot(), None, None, NOW, 600);
        assert_eq!(content, LndContent::unreachable());
        assert_eq!(tags, None);
    }

    #[test]
    fn lnd_view_falls_back_to_the_boot_network() {
        let state = LndSyncState::new();
        state.observe(observation(true, None), NOW);
        let (_, tags) = lnd_view(&state.snapshot(), None, Some("regtest"), NOW, 600);
        assert_eq!(tags.map(|t| t.network).as_deref(), Some("regtest"));
    }

    async fn insert(pool: &SqlitePool, status: &str, failed: bool, attempts: i64, settled_at: i64) {
        sqlx::query(
            "INSERT INTO orders (id, kind, event_id, status, premium, payment_method, amount, \
             fiat_code, fiat_amount, created_at, expires_at, failed_payment, payment_attempts, \
             invoice_held_at, hold_settled_at) \
             VALUES (?, 'sell', 'e', ?, 0, 'cash', 1000, 'USD', 10, 0, 0, ?, ?, ?, ?)",
        )
        .bind(uuid::Uuid::new_v4())
        .bind(status)
        .bind(failed)
        .bind(attempts)
        .bind(settled_at - 3_600)
        .bind(settled_at)
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn payout_rows_split_settled_orders_by_who_they_wait_on() {
        let pool = pool().await;
        // Still being paid by the daemon (one deferred, one retrying).
        insert(&pool, "settled-hold-invoice", false, 0, NOW - 1_000).await;
        insert(&pool, "settled-hold-invoice", true, 1, NOW - 5_000).await;
        // Attempts used up: waiting on the buyer's new invoice.
        insert(&pool, "settled-hold-invoice", true, 3, NOW - 90_000).await;
        // Not settled-hold-invoice.
        insert(&pool, "success", false, 0, NOW - 100_000).await;

        let rows = payout_rows(&pool, 3, NOW).await.unwrap();
        assert_eq!(
            rows,
            PayoutRows {
                settled_unpaid: 2,
                oldest_settled_unpaid_age_secs: Some(5_000),
                awaiting_buyer_invoice: 1,
            }
        );
    }

    #[tokio::test]
    async fn payout_rows_without_settled_orders() {
        let pool = pool().await;
        assert_eq!(
            payout_rows(&pool, 3, NOW).await.unwrap(),
            PayoutRows::default()
        );
    }

    #[tokio::test]
    async fn nothing_is_published_without_a_connected_relay() {
        // The test client has no relay: the first evaluation at boot looks
        // like this before the relays connect.
        let pool = pool().await;
        let ctx = TestContextBuilder::new()
            .with_pool(Arc::new(pool.clone()))
            .with_settings(test_settings())
            .build();
        let mut publisher = Publisher::new(&ctx).await;

        publisher.tick(&ctx).await;

        assert!(publisher.last_publish.is_none());
        assert_eq!(load_persisted(&pool).await.unwrap().last_stamp, None);
    }

    #[tokio::test]
    async fn evaluate_cashu_reports_relays_and_database_only() {
        let pool = pool().await;
        let mut settings = test_settings();
        settings.cashu = Some(crate::config::types::CashuSettings {
            enabled: true,
            ..Default::default()
        });
        let ctx = TestContextBuilder::new()
            .with_pool(Arc::new(pool))
            .with_settings(settings)
            .build();
        let mut publisher = Publisher::new(&ctx).await;

        let evaluation = publisher.evaluate(&ctx, NOW).await;

        let names: Vec<_> = evaluation
            .report
            .checks
            .iter()
            .map(|(name, _, _)| *name)
            .collect();
        assert_eq!(
            names,
            vec![CheckName::CashuMint, CheckName::Relays, CheckName::Database]
        );
        assert_eq!(evaluation.report.escrow_mode, "cashu");
        assert_eq!(evaluation.report.lnd, None);
        assert_eq!(evaluation.content.lnd, None);
        assert_eq!(evaluation.content.payouts, None);
        assert_eq!(evaluation.content.dev_fee, None);
        // No relay in a test client: the instance cannot talk to anyone.
        assert_eq!(evaluation.report.status, HealthStatus::Down);
    }

    #[tokio::test]
    async fn evaluate_lightning_with_an_unknown_lnd_is_down_but_has_objects() {
        let pool = pool().await;
        let ctx = TestContextBuilder::new()
            .with_pool(Arc::new(pool))
            .with_settings(test_settings())
            .build();
        let mut publisher = Publisher::new(&ctx).await;

        let evaluation = publisher.evaluate(&ctx, NOW).await;

        let checks: Vec<_> = evaluation
            .report
            .checks
            .iter()
            .map(|(name, state, _)| (*name, *state))
            .collect();
        assert_eq!(checks[0], (CheckName::LndSync, CheckState::Unknown));
        assert_eq!(checks[1], (CheckName::Payouts, CheckState::Ok));
        assert_eq!(checks[3], (CheckName::Database, CheckState::Ok));
        assert_eq!(evaluation.report.status, HealthStatus::Down);
        assert_eq!(evaluation.report.escrow_mode, "lightning");
        assert_eq!(evaluation.report.lnd, None);
        // Lightning: `lnd` and `payouts` are objects even with LND unknown.
        assert_eq!(evaluation.content.lnd, Some(LndContent::unreachable()));
        let payouts = evaluation.content.payouts.expect("an object on lightning");
        assert_eq!(payouts.settled_unpaid_orders, Some(0));
        assert_eq!(payouts.awaiting_buyer_invoice_orders, Some(0));
    }

    /// End to end against a local relay: a synced Lightning instance with a
    /// watchdog key publishes a valid event whose content the watchdog can
    /// decrypt, and only once per interval.
    #[tokio::test]
    async fn publishes_a_valid_health_event_to_the_relays() {
        use nostr_sdk::local_relay::MockRelay;
        use nostr_sdk::prelude::{nip44, Client, Filter, Keys, Kind};

        // `LndConnector::new` reads the global settings; it then fails to
        // reach an LND, and the ListChannels fields are null.
        crate::config::init_test_nostr_keys();
        let _ = crate::config::MOSTRO_CONFIG.set(test_settings());
        let relay = MockRelay::run().await.expect("mock relay");
        let client = Client::default();
        client.add_relay(relay.url().await).await.unwrap();
        client.connect().and_wait(Duration::from_secs(5)).await;

        let keys = Keys::generate();
        let watchdog = Keys::generate();
        let mut settings = test_settings();
        settings.health.watchdog_pubkey = watchdog.public_key().to_hex();
        let lnd = LndSyncState::new();
        let now = Utc::now().timestamp();
        lnd.observe(observation(true, Some("regtest")), now);
        let pool = pool().await;
        let ctx = TestContextBuilder::new()
            .with_pool(Arc::new(pool.clone()))
            .with_settings(settings)
            .with_nostr_client(client.clone())
            .with_keys(keys.clone())
            .build()
            .with_lnd_sync(lnd);

        let mut publisher = Publisher::new(&ctx).await;
        publisher.tick(&ctx).await;
        publisher.tick(&ctx).await;

        let events = client
            .fetch_events(
                Filter::new()
                    .kind(Kind::Custom(38389))
                    .author(keys.public_key()),
            )
            .timeout(Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(events.len(), 1, "one publication per interval");
        let event = events.first().unwrap();
        event.verify().unwrap();
        let tag = |name: &str| {
            event
                .tags
                .iter()
                .map(|t| t.as_slice())
                .find(|t| t[0] == name)
                .map(|t| t[1].clone())
        };
        assert_eq!(tag("d"), Some(keys.public_key().to_hex()));
        assert_eq!(tag("seq").as_deref(), Some("1"));
        assert_eq!(tag("lnd_network").as_deref(), Some("regtest"));
        assert_eq!(tag("p"), Some(watchdog.public_key().to_hex()));
        // Synced LND, one relay out of one, database fine, no payouts.
        assert_eq!(tag("status").as_deref(), Some("ok"));

        let plaintext =
            nip44::decrypt(watchdog.secret_key(), &keys.public_key(), &event.content).unwrap();
        let json: serde_json::Value = serde_json::from_str(&plaintext).unwrap();
        assert_eq!(json["v"], 1);
        assert_eq!(json["lnd"]["reachable"], true);
        assert_eq!(json["lnd"]["block_height"], 971_349);
        assert_eq!(json["relays"]["configured"], 1);
        assert_eq!(json["relays"]["connected"], 1);
        assert!(json["payouts"].is_object());
        assert_eq!(json["maintenance"]["enabled"], false);

        // seq and created_at were persisted for the next run.
        let persisted = load_persisted(&pool).await.unwrap();
        assert_eq!(persisted.last_stamp.map(|s| s.seq), Some(1));
        assert_eq!(
            persisted.last_stamp.map(|s| s.created_at),
            Some(event.created_at.as_secs() as i64)
        );
    }

    #[tokio::test]
    async fn a_state_change_republishes_after_ten_seconds_only() {
        let pool = pool().await;
        let ctx = TestContextBuilder::new()
            .with_pool(Arc::new(pool))
            .with_settings(test_settings())
            .build();
        let mut publisher = Publisher::new(&ctx).await;
        let evaluation = publisher.evaluate(&ctx, NOW).await;
        assert!(publisher.should_publish(&evaluation), "first evaluation");

        publisher.last_publish = Some(Instant::now());
        publisher.last_signature = Some(evaluation.signature());
        assert!(!publisher.should_publish(&evaluation), "nothing changed");

        let mut changed = evaluation.clone();
        changed.report.maintenance = !changed.report.maintenance;
        assert!(!publisher.should_publish(&changed), "within 10s");

        publisher.last_publish = Some(Instant::now() - Duration::from_secs(MIN_REPUBLISH_SECS));
        assert!(publisher.should_publish(&changed));
        assert!(!publisher.should_publish(&evaluation));

        publisher.last_publish = Some(Instant::now() - Duration::from_secs(publisher.interval));
        assert!(publisher.should_publish(&evaluation), "interval elapsed");
    }
}
