//! LND chain-sync tracking and the fail-closed maintenance mode (#1063).
//!
//! On 2026-10-14 a production LND got stuck on a stale chain for about 21
//! hours. With a stale block height every outgoing HTLC carried a CLTV the
//! receiver rejected, so every buyer payout and dev fee failed, while hold
//! invoices and incoming payments kept working and nobody noticed.
//!
//! [`LndSyncState`] is fed by `job_lnd_sync_monitor` (src/scheduler.rs),
//! which polls `GetInfo` every [`LND_SYNC_POLL_SECS`]. Three things read it:
//!
//! - [`LndSyncSnapshot::defers_outgoing_payments`]: payouts, dev fees and bond
//!   payouts wait instead of failing, so a buyer never loses a payment
//!   attempt (and is never asked for a new invoice) over our own node's
//!   problem.
//! - [`auto_maintenance_decision`] / [`apply_auto_maintenance`]: once LND has
//!   been unsynced for `[health].lnd_unsynced_fail_after` seconds, the daemon
//!   enters maintenance with reason [`REASON_LND_UNSYNCED`], so no new escrow
//!   is opened that it could not pay out, and lifts it again once LND has been
//!   synced for `[health].lnd_resynced_clear_after` seconds, but only if the
//!   maintenance is still its own (`KEY_AUTOMATIC`, never inferred from the
//!   reason text).
//! - [`LndSyncSnapshot::sync_check`]: the `lnd_sync` check, with the
//!   definition of the health event (kind 38389); here it drives the
//!   automatic maintenance.

use crate::app::context::AppContext;
use crate::app::health::check::CheckState;
use crate::app::maintenance::REASON_LND_UNSYNCED;
use crate::config::types::HealthSettings;
use fedimint_tonic_lnd::lnrpc::GetInfoResponse;
use mostro_core::error::MostroError;
use std::sync::{Arc, RwLock};
use tracing::{info, warn};

/// How often `job_lnd_sync_monitor` asks LND for `GetInfo`, seconds.
pub const LND_SYNC_POLL_SECS: u64 = 15;

/// How long LND must have been unsynced (or unreachable) without
/// interruption before outgoing payments are deferred, seconds. LND reports
/// `synced_to_chain = false` for a few seconds whenever a block arrives;
/// that must not hold payouts back.
pub const PAYMENT_DEFER_AFTER_SECS: i64 = 60;

/// An observation older than this is not trusted: if the monitor itself
/// stopped, nothing is deferred and every check that depends on it is
/// `unknown`. Three missed polls.
const OBSERVATION_STALE_AFTER_SECS: i64 = 3 * LND_SYNC_POLL_SECS as i64;

/// What one successful `GetInfo` said.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LndObservation {
    pub synced_to_chain: bool,
    pub block_height: u32,
    pub best_header_timestamp: i64,
    /// The `network` of the first `chains` entry (`mainnet`, `testnet`, ...).
    pub network: Option<String>,
    pub peers: u32,
    pub channels_active: u32,
    pub channels_inactive: u32,
    pub channels_pending: u32,
}

impl From<&GetInfoResponse> for LndObservation {
    fn from(info: &GetInfoResponse) -> Self {
        Self {
            synced_to_chain: info.synced_to_chain,
            block_height: info.block_height,
            best_header_timestamp: info.best_header_timestamp,
            network: info
                .chains
                .first()
                .map(|chain| chain.network.clone())
                .filter(|network| !network.is_empty()),
            peers: info.num_peers,
            channels_active: info.num_active_channels,
            channels_inactive: info.num_inactive_channels,
            channels_pending: info.num_pending_channels,
        }
    }
}

/// A change worth logging, returned by the `observe*` methods.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncTransition {
    BecameUnsynced,
    BecameSynced,
    BecameUnreachable,
    BecameReachable,
}

/// The tracked state. All times are unix seconds.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LndSyncSnapshot {
    /// When the last observation (successful or not) was made. `None` until
    /// the monitor has run once.
    pub observed_at: Option<i64>,
    /// The last successful `GetInfo`; `None` while LND is unreachable.
    pub last: Option<LndObservation>,
    /// When the current uninterrupted run of failed `GetInfo` calls began.
    pub unreachable_since: Option<i64>,
    /// When the current uninterrupted run of `synced_to_chain = false`
    /// began. Kept across an unreachable spell: a node that was stuck before
    /// it stopped answering is not presumed fixed when it answers again.
    pub unsynced_since: Option<i64>,
    /// When the current uninterrupted run of `synced_to_chain = true` began.
    /// Reset by an unreachable spell.
    pub synced_since: Option<i64>,
    /// When the current uninterrupted run of "cannot pay" began: unsynced
    /// or unreachable, in any mix. Only a synced observation ends it, so
    /// moving between the two unhealthy states never restarts the deferral
    /// clock and lets payments out for another 60 seconds.
    pub unhealthy_since: Option<i64>,
    /// When the last spell long enough to defer payments ended (the synced
    /// observation that closed it). Lets a failure that arrives after LND
    /// recovered be recognised as caused by that spell.
    pub deferral_ended_at: Option<i64>,
}

/// Whether at least `threshold` seconds passed between `since` and `now`,
/// compared as `u64` so a threshold above `i64::MAX` can never wrap to a
/// negative number and be met at once.
fn elapsed_at_least(since: i64, now: i64, threshold: u64) -> bool {
    u64::try_from(now.saturating_sub(since)).is_ok_and(|elapsed| elapsed >= threshold)
}

impl LndSyncSnapshot {
    fn observe(&mut self, observation: LndObservation, now: i64) -> Option<SyncTransition> {
        let was_reachable = self.observed_at.is_none() || self.last.is_some();
        let was_synced = self.last.as_ref().map(|last| last.synced_to_chain);
        self.observed_at = Some(now);
        self.unreachable_since = None;
        let transition = if observation.synced_to_chain {
            if self
                .unhealthy_since
                .is_some_and(|since| now.saturating_sub(since) >= PAYMENT_DEFER_AFTER_SECS)
            {
                self.deferral_ended_at = Some(now);
            }
            self.unsynced_since = None;
            self.unhealthy_since = None;
            self.synced_since = self.synced_since.or(Some(now));
            (was_synced == Some(false)).then_some(SyncTransition::BecameSynced)
        } else {
            self.synced_since = None;
            self.unsynced_since = self.unsynced_since.or(Some(now));
            self.unhealthy_since = self.unhealthy_since.or(Some(now));
            (was_synced != Some(false)).then_some(SyncTransition::BecameUnsynced)
        };
        self.last = Some(observation);
        if !was_reachable {
            return Some(SyncTransition::BecameReachable);
        }
        transition
    }

    fn observe_unreachable(&mut self, now: i64) -> Option<SyncTransition> {
        let was_reachable = self.observed_at.is_none() || self.last.is_some();
        self.observed_at = Some(now);
        self.last = None;
        self.synced_since = None;
        self.unreachable_since = self.unreachable_since.or(Some(now));
        self.unhealthy_since = self.unhealthy_since.or(Some(now));
        was_reachable.then_some(SyncTransition::BecameUnreachable)
    }

    /// Whether the last observation is recent enough to act on.
    fn is_fresh(&self, now: i64) -> bool {
        self.observed_at
            .is_some_and(|at| now.saturating_sub(at) <= OBSERVATION_STALE_AFTER_SECS)
    }

    /// The `lnd_sync` check: `ok` while synced, `warn` while unsynced for
    /// less than `fail_after` seconds, `fail` after that, `unknown` when LND
    /// cannot be reached or nothing recent is known.
    pub fn sync_check(&self, now: i64, fail_after: u64) -> CheckState {
        if !self.is_fresh(now) {
            return CheckState::Unknown;
        }
        match (&self.last, self.unsynced_since) {
            (None, _) => CheckState::Unknown,
            (Some(last), _) if last.synced_to_chain => CheckState::Ok,
            (Some(_), Some(since)) if elapsed_at_least(since, now, fail_after) => CheckState::Fail,
            (Some(_), _) => CheckState::Warn,
        }
    }

    /// Whether outgoing payments must wait: LND has been unsynced or
    /// unreachable, in any mix, for [`PAYMENT_DEFER_AFTER_SECS`] without a
    /// synced observation in between. A stale observation never defers
    /// anything, so a dead monitor cannot hold payouts back.
    pub fn defers_outgoing_payments(&self, now: i64) -> bool {
        if !self.is_fresh(now) {
            return false;
        }
        let healthy = self.last.as_ref().is_some_and(|last| last.synced_to_chain);
        !healthy
            && self
                .unhealthy_since
                .is_some_and(|since| now.saturating_sub(since) >= PAYMENT_DEFER_AFTER_SECS)
    }

    /// Whether payments were deferred at any moment between `started_at` and
    /// `now`: they are now, or a deferring spell ended after `started_at`.
    /// A payment sent at `started_at` that fails later is then our node's
    /// fault, even if LND has recovered by the time the failure arrives.
    pub fn deferred_since(&self, started_at: i64, now: i64) -> bool {
        self.defers_outgoing_payments(now)
            || self
                .deferral_ended_at
                .is_some_and(|ended_at| ended_at >= started_at)
    }

    /// Seconds LND has been synced without interruption, if it is now.
    pub fn synced_for(&self, now: i64) -> Option<i64> {
        if !self.is_fresh(now) {
            return None;
        }
        match &self.last {
            Some(last) if last.synced_to_chain => {
                self.synced_since.map(|since| now.saturating_sub(since))
            }
            _ => None,
        }
    }
}

/// Process-wide LND sync state. Cheap to clone; every clone observes the
/// same state. A fresh state knows nothing and defers nothing.
#[derive(Clone, Debug, Default)]
pub struct LndSyncState {
    inner: Arc<RwLock<LndSyncSnapshot>>,
}

impl LndSyncState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a successful `GetInfo`.
    pub fn observe(&self, observation: LndObservation, now: i64) -> Option<SyncTransition> {
        self.write().observe(observation, now)
    }

    /// Seed the state with the `GetInfo` mostrod made at startup, before any
    /// job that sends payments starts. The grace period that keeps a block's
    /// few seconds of `synced_to_chain = false` from holding payouts back
    /// does not apply here: at startup there is no telling how long the node
    /// has been unsynced, and a burst of retries of persisted failed payouts
    /// against a stale chain would burn the buyers' attempts. If it is
    /// unsynced, payments wait from the first moment; a node that was only
    /// between blocks reports synced on the next poll, 15 s later.
    pub fn observe_at_startup(&self, observation: LndObservation, now: i64) {
        let mut state = self.write();
        let synced = observation.synced_to_chain;
        state.observe(observation, now);
        if !synced {
            state.unhealthy_since = Some(now.saturating_sub(PAYMENT_DEFER_AFTER_SECS));
        }
    }

    /// Like [`LndSyncState::observe_at_startup`], when the startup `GetInfo`
    /// failed: payments wait from the first moment.
    pub fn observe_unreachable_at_startup(&self, now: i64) {
        let mut state = self.write();
        state.observe_unreachable(now);
        state.unhealthy_since = Some(now.saturating_sub(PAYMENT_DEFER_AFTER_SECS));
    }

    /// Record a failed or timed-out `GetInfo`.
    pub fn observe_unreachable(&self, now: i64) -> Option<SyncTransition> {
        self.write().observe_unreachable(now)
    }

    /// A copy of the current state.
    pub fn snapshot(&self) -> LndSyncSnapshot {
        match self.inner.read() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// Shortcut for [`LndSyncSnapshot::defers_outgoing_payments`] at `now`.
    pub fn defers_outgoing_payments(&self, now: i64) -> bool {
        self.snapshot().defers_outgoing_payments(now)
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, LndSyncSnapshot> {
        match self.inner.write() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

/// Log a transition at the level an operator needs to see it.
pub fn log_transition(transition: SyncTransition, snapshot: &LndSyncSnapshot) {
    let height = snapshot.last.as_ref().map(|last| last.block_height);
    match transition {
        SyncTransition::BecameUnsynced => warn!(
            "LND reports synced_to_chain = false (block height {:?}); outgoing payments wait after {}s",
            height, PAYMENT_DEFER_AFTER_SECS
        ),
        SyncTransition::BecameSynced => info!("LND is synced to chain again (block height {height:?})"),
        SyncTransition::BecameUnreachable => {
            warn!("LND did not answer GetInfo; outgoing payments wait after {PAYMENT_DEFER_AFTER_SECS}s")
        }
        SyncTransition::BecameReachable => info!("LND answers GetInfo again"),
    }
}

/// What the fail-closed rule wants done with maintenance mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AutoMaintenance {
    /// Enter maintenance with reason [`REASON_LND_UNSYNCED`].
    Enable,
    /// Leave maintenance, if it is still the daemon's own (turned on with
    /// [`REASON_LND_UNSYNCED`] and not taken over by an operator since).
    Disable,
    Nothing,
}

/// The fail-closed rule. Pure, so it is under test without a node.
pub fn auto_maintenance_decision(
    snapshot: &LndSyncSnapshot,
    now: i64,
    settings: &HealthSettings,
    maintenance_enabled: bool,
) -> AutoMaintenance {
    if !settings.auto_maintenance_on_lnd_unsynced {
        return AutoMaintenance::Nothing;
    }
    if !maintenance_enabled
        && snapshot.sync_check(now, settings.lnd_unsynced_fail_after) == CheckState::Fail
    {
        return AutoMaintenance::Enable;
    }
    if maintenance_enabled
        && snapshot.synced_for(now).is_some_and(|secs| {
            u64::try_from(secs).is_ok_and(|secs| secs >= settings.lnd_resynced_clear_after)
        })
    {
        return AutoMaintenance::Disable;
    }
    AutoMaintenance::Nothing
}

/// Apply [`auto_maintenance_decision`] to the context's maintenance flag.
/// Returns what was actually changed: `Disable` is only reported when the
/// maintenance was still the daemon's own.
pub async fn apply_auto_maintenance(
    ctx: &AppContext,
    now: i64,
) -> Result<AutoMaintenance, MostroError> {
    let snapshot = ctx.lnd_sync().snapshot();
    let maintenance = ctx.maintenance();
    match auto_maintenance_decision(
        &snapshot,
        now,
        &ctx.settings().health,
        maintenance.is_enabled(),
    ) {
        AutoMaintenance::Enable => {
            if maintenance
                .enable_unless_enabled(ctx.pool(), REASON_LND_UNSYNCED)
                .await?
            {
                warn!(
                    "LND has not been synced to chain for {}s or more: maintenance mode ON \
                     (reason {REASON_LND_UNSYNCED}); new orders and takes are rejected until it recovers",
                    ctx.settings().health.lnd_unsynced_fail_after
                );
                return Ok(AutoMaintenance::Enable);
            }
            Ok(AutoMaintenance::Nothing)
        }
        AutoMaintenance::Disable => {
            if maintenance.disable_if_automatic(ctx.pool()).await? {
                warn!(
                    "LND has been synced to chain for {}s: maintenance mode OFF (it was turned on by the daemon)",
                    ctx.settings().health.lnd_resynced_clear_after
                );
                return Ok(AutoMaintenance::Disable);
            }
            Ok(AutoMaintenance::Nothing)
        }
        AutoMaintenance::Nothing => Ok(AutoMaintenance::Nothing),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: i64 = 1_792_000_000;

    fn observation(synced: bool) -> LndObservation {
        LndObservation {
            synced_to_chain: synced,
            block_height: 971_349,
            best_header_timestamp: T0 - 30,
            network: Some("mainnet".to_string()),
            peers: 19,
            channels_active: 8,
            channels_inactive: 1,
            channels_pending: 0,
        }
    }

    /// Observe `synced` every poll from `from` to `to` inclusive.
    fn poll(state: &LndSyncState, synced: bool, from: i64, to: i64) {
        let mut now = from;
        while now <= to {
            state.observe(observation(synced), now);
            now += LND_SYNC_POLL_SECS as i64;
        }
    }

    #[test]
    fn lnd_sync_check_is_unknown_before_the_first_observation() {
        let state = LndSyncState::new();
        assert_eq!(state.snapshot().sync_check(T0, 600), CheckState::Unknown);
        assert!(!state.defers_outgoing_payments(T0));
    }

    #[test]
    fn lnd_sync_check_fails_after_threshold() {
        let state = LndSyncState::new();
        poll(&state, false, T0, T0 + 590);
        let snapshot = state.snapshot();
        assert_eq!(snapshot.unsynced_since, Some(T0));
        assert_eq!(snapshot.sync_check(T0 + 590, 600), CheckState::Warn);
        assert_eq!(snapshot.sync_check(T0 + 600, 600), CheckState::Fail);
    }

    #[test]
    fn a_synced_observation_restarts_the_unsynced_clock() {
        let state = LndSyncState::new();
        poll(&state, false, T0, T0 + 300);
        state.observe(observation(true), T0 + 315);
        poll(&state, false, T0 + 330, T0 + 900);
        let snapshot = state.snapshot();
        assert_eq!(snapshot.unsynced_since, Some(T0 + 330));
        assert_eq!(snapshot.sync_check(T0 + 900, 600), CheckState::Warn);
    }

    #[test]
    fn lnd_sync_check_is_ok_while_synced_and_unknown_while_unreachable() {
        let state = LndSyncState::new();
        state.observe(observation(true), T0);
        assert_eq!(state.snapshot().sync_check(T0, 600), CheckState::Ok);
        state.observe_unreachable(T0 + 15);
        assert_eq!(
            state.snapshot().sync_check(T0 + 15, 600),
            CheckState::Unknown
        );
    }

    #[test]
    fn a_stale_observation_is_unknown_and_defers_nothing() {
        let state = LndSyncState::new();
        poll(&state, false, T0, T0 + 900);
        // The monitor stopped: the last word is too old to act on.
        let later = T0 + 900 + OBSERVATION_STALE_AFTER_SECS + 1;
        assert_eq!(state.snapshot().sync_check(later, 600), CheckState::Unknown);
        assert!(!state.defers_outgoing_payments(later));
    }

    #[test]
    fn payments_defer_only_after_a_sustained_unsynced_spell() {
        let state = LndSyncState::new();
        // A block arrives: a few seconds of `false` must not hold payouts.
        state.observe(observation(false), T0);
        assert!(!state.defers_outgoing_payments(T0 + 10));
        poll(&state, false, T0 + 15, T0 + 60);
        assert!(state.defers_outgoing_payments(T0 + 60));
        state.observe(observation(true), T0 + 75);
        assert!(!state.defers_outgoing_payments(T0 + 75));
    }

    #[test]
    fn payments_defer_while_lnd_is_unreachable() {
        let state = LndSyncState::new();
        state.observe(observation(true), T0);
        state.observe_unreachable(T0 + 15);
        assert!(!state.defers_outgoing_payments(T0 + 30));
        state.observe_unreachable(T0 + 75);
        assert!(state.defers_outgoing_payments(T0 + 75));
    }

    /// Codex review on #1065: going from unsynced to unreachable (or back)
    /// once payments are deferred must not let them out again for 60 s.
    #[test]
    fn deferral_survives_moving_between_unsynced_and_unreachable() {
        let state = LndSyncState::new();
        poll(&state, false, T0, T0 + 45);
        state.observe_unreachable(T0 + 60);
        assert!(
            state.defers_outgoing_payments(T0 + 60),
            "60s unhealthy across both states"
        );
        state.observe(observation(false), T0 + 75);
        assert!(state.defers_outgoing_payments(T0 + 75), "back to unsynced");
        state.observe_unreachable(T0 + 90);
        assert!(state.defers_outgoing_payments(T0 + 90), "unreachable again");
        // Only a synced observation ends the spell.
        state.observe(observation(true), T0 + 105);
        assert!(!state.defers_outgoing_payments(T0 + 105));
        state.observe_unreachable(T0 + 120);
        assert!(
            !state.defers_outgoing_payments(T0 + 120),
            "a new spell starts its own clock"
        );
        assert_eq!(state.snapshot().unhealthy_since, Some(T0 + 120));
    }

    /// Codex review on #1065: at startup the retry job must not dispatch
    /// persisted failed payouts against an unsynced node, neither before
    /// the monitor's first poll nor during the usual 60 s grace period.
    #[test]
    fn an_unsynced_node_at_startup_defers_payments_at_once() {
        let state = LndSyncState::new();
        state.observe_at_startup(observation(false), T0);
        assert!(state.defers_outgoing_payments(T0));
        // The lnd_sync check itself still dates the spell from startup.
        assert_eq!(state.snapshot().sync_check(T0, 600), CheckState::Warn);
        // The monitor's polls keep it deferred, and a synced poll ends it.
        state.observe(observation(false), T0 + 15);
        assert!(state.defers_outgoing_payments(T0 + 15));
        state.observe(observation(true), T0 + 30);
        assert!(!state.defers_outgoing_payments(T0 + 30));
    }

    #[test]
    fn an_unreachable_node_at_startup_defers_payments_at_once() {
        let state = LndSyncState::new();
        state.observe_unreachable_at_startup(T0);
        assert!(state.defers_outgoing_payments(T0));
        state.observe(observation(true), T0 + 15);
        assert!(!state.defers_outgoing_payments(T0 + 15));
    }

    #[test]
    fn a_synced_node_at_startup_defers_nothing() {
        let state = LndSyncState::new();
        state.observe_at_startup(observation(true), T0);
        assert!(!state.defers_outgoing_payments(T0));
        assert_eq!(state.snapshot().sync_check(T0, 600), CheckState::Ok);
    }

    /// Codex review on #1065: a payout sent before LND went bad and failed
    /// after it recovered was still caused by the spell.
    #[test]
    fn a_deferring_spell_during_a_send_is_remembered_after_recovery() {
        let state = LndSyncState::new();
        state.observe(observation(true), T0);
        let sent_at = T0 + 5;
        poll(&state, false, T0 + 15, T0 + 90);
        state.observe(observation(true), T0 + 105);
        let snapshot = state.snapshot();
        assert!(!snapshot.defers_outgoing_payments(T0 + 110));
        assert!(snapshot.deferred_since(sent_at, T0 + 110));
        // A send started after the spell ended is judged on its own.
        assert!(!snapshot.deferred_since(T0 + 106, T0 + 110));
    }

    #[test]
    fn a_short_blip_during_a_send_is_not_a_deferral() {
        let state = LndSyncState::new();
        state.observe(observation(true), T0);
        state.observe(observation(false), T0 + 15);
        state.observe(observation(true), T0 + 30);
        assert!(!state.snapshot().deferred_since(T0, T0 + 30));
    }

    #[test]
    fn transitions_are_reported_once() {
        let state = LndSyncState::new();
        assert_eq!(state.observe(observation(true), T0), None);
        assert_eq!(
            state.observe(observation(false), T0 + 15),
            Some(SyncTransition::BecameUnsynced)
        );
        assert_eq!(state.observe(observation(false), T0 + 30), None);
        assert_eq!(
            state.observe_unreachable(T0 + 45),
            Some(SyncTransition::BecameUnreachable)
        );
        assert_eq!(state.observe_unreachable(T0 + 60), None);
        assert_eq!(
            state.observe(observation(true), T0 + 75),
            Some(SyncTransition::BecameReachable)
        );
        assert_eq!(state.observe(observation(true), T0 + 90), None);
        assert_eq!(
            state.observe(observation(false), T0 + 105),
            Some(SyncTransition::BecameUnsynced)
        );
        assert_eq!(
            state.observe(observation(true), T0 + 120),
            Some(SyncTransition::BecameSynced)
        );
    }

    #[test]
    fn an_unreachable_spell_keeps_the_unsynced_clock_but_resets_the_synced_one() {
        let state = LndSyncState::new();
        poll(&state, false, T0, T0 + 300);
        state.observe_unreachable(T0 + 315);
        state.observe(observation(false), T0 + 330);
        assert_eq!(state.snapshot().unsynced_since, Some(T0));

        poll(&state, true, T0 + 345, T0 + 600);
        state.observe_unreachable(T0 + 615);
        state.observe(observation(true), T0 + 630);
        assert_eq!(state.snapshot().synced_since, Some(T0 + 630));
    }

    #[test]
    fn get_info_maps_to_an_observation() {
        use fedimint_tonic_lnd::lnrpc::Chain;

        #[allow(deprecated)]
        let info = GetInfoResponse {
            synced_to_chain: false,
            block_height: 971_349,
            best_header_timestamp: 1_791_951_597,
            num_peers: 19,
            num_active_channels: 8,
            num_inactive_channels: 1,
            num_pending_channels: 2,
            chains: vec![Chain {
                chain: "bitcoin".to_string(),
                network: "mainnet".to_string(),
            }],
            ..Default::default()
        };
        assert_eq!(
            LndObservation::from(&info),
            LndObservation {
                synced_to_chain: false,
                block_height: 971_349,
                best_header_timestamp: 1_791_951_597,
                network: Some("mainnet".to_string()),
                peers: 19,
                channels_active: 8,
                channels_inactive: 1,
                channels_pending: 2,
            }
        );
        assert_eq!(
            LndObservation::from(&GetInfoResponse::default()).network,
            None
        );
    }

    fn settings() -> HealthSettings {
        HealthSettings::default()
    }

    #[test]
    fn auto_maintenance_enabled_when_lnd_unsynced_past_threshold() {
        let state = LndSyncState::new();
        poll(&state, false, T0, T0 + 600);
        let snapshot = state.snapshot();
        assert_eq!(
            auto_maintenance_decision(&snapshot, T0 + 585, &settings(), false),
            AutoMaintenance::Nothing,
            "still within the threshold"
        );
        assert_eq!(
            auto_maintenance_decision(&snapshot, T0 + 600, &settings(), false),
            AutoMaintenance::Enable
        );
        // Already on (for whatever reason): nothing to do.
        assert_eq!(
            auto_maintenance_decision(&snapshot, T0 + 600, &settings(), true),
            AutoMaintenance::Nothing
        );
    }

    #[test]
    fn auto_maintenance_lifted_only_after_a_stable_resync() {
        let state = LndSyncState::new();
        poll(&state, true, T0, T0 + 300);
        let snapshot = state.snapshot();
        assert_eq!(
            auto_maintenance_decision(&snapshot, T0 + 285, &settings(), true),
            AutoMaintenance::Nothing
        );
        assert_eq!(
            auto_maintenance_decision(&snapshot, T0 + 300, &settings(), true),
            AutoMaintenance::Disable
        );
        assert_eq!(
            auto_maintenance_decision(&snapshot, T0 + 300, &settings(), false),
            AutoMaintenance::Nothing
        );
    }

    #[test]
    fn auto_maintenance_can_be_turned_off() {
        let state = LndSyncState::new();
        poll(&state, false, T0, T0 + 900);
        let off = HealthSettings {
            auto_maintenance_on_lnd_unsynced: false,
            ..settings()
        };
        assert_eq!(
            auto_maintenance_decision(&state.snapshot(), T0 + 900, &off, false),
            AutoMaintenance::Nothing
        );
    }

    #[test]
    fn auto_maintenance_ignores_an_unreachable_node() {
        // `unknown` is not `fail`: the spec only fails closed on a node that
        // answers and says it is not synced.
        let state = LndSyncState::new();
        state.observe_unreachable(T0);
        let mut now = T0;
        while now <= T0 + 900 {
            state.observe_unreachable(now);
            now += 15;
        }
        assert_eq!(
            auto_maintenance_decision(&state.snapshot(), T0 + 900, &settings(), false),
            AutoMaintenance::Nothing
        );
    }
}
