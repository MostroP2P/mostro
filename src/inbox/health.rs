//! Whether the daemon's ear is open, and when it was not.
//!
//! [`InboxHealth`] is the half of the inbox the rest of the daemon reads. The
//! subscription machinery next door ([`super`]) decides what is true — which
//! relays answered the REQ, which stopped — and records it here; the scheduler
//! asks this record whether Mostro was in a position to hear at all before it
//! acts on a user's silence.
//!
//! Two things live here for that reason. The **outage log**: every stretch
//! during which no relay was serving the inbox, kept so an order can be
//! credited for exactly the downtime that overlaps its own wait. And the
//! **per-relay pacing**: which relays have acknowledged the subscription, and
//! when another REQ may go out to one that has not — shared by the event loop
//! and the watchdog so neither can bypass the other's backoff.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use nostr_sdk::prelude::*;

/// Delay before a *second* consecutive re-subscribe to the same relay.
///
/// The first `CLOSED` is answered immediately — the common case is a transient
/// refusal, and every second of delay is a second of deaf node. Backoff only
/// starts mattering when a relay keeps closing the inbox.
const RESUBSCRIBE_INITIAL_BACKOFF: Duration = Duration::from_secs(2);

/// Ceiling for the per-relay re-subscribe delay.
///
/// A relay that has refused the inbox for five minutes straight is not having
/// a hiccup — it is configured to refuse us (NIP-42, a pubkey allowlist, a ban)
/// and the operator has to intervene. Retrying every five minutes keeps the
/// door open for a config change on their side without generating traffic that
/// looks like an attack.
///
/// This is a real ceiling because [`super::check_inbox_health`] draws on the
/// same per-relay budget rather than re-sending on every pass: an audit every
/// `INBOX_WATCHDOG_INTERVAL` would otherwise put a hard floor of thirty
/// seconds under it. The doublings still start well below that interval, so a
/// relay that merely lost the inbox is re-subscribed on the next pass and only
/// a persistently refusing one reaches this figure.
const RESUBSCRIBE_MAX_BACKOFF: Duration = Duration::from_secs(300);

/// Per-relay re-subscribe pacing.
#[derive(Debug)]
struct RelayBackoff {
    /// Earliest instant at which another REQ may go out to this relay.
    next_attempt_at: Instant,
    /// Delay applied after the next attempt; doubles up to the ceiling.
    delay: Duration,
}

/// Whether the daemon can currently hear anything at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InboxStatus {
    /// At least one connected relay is serving the inbox subscription.
    Listening,
    /// No connected relay is serving it: every message sent to Mostro right
    /// now is being lost.
    Blind,
}

/// Process-wide inbox health. `None` until [`InboxHealth::install_global`]
/// runs at startup; consumers treat an absent health record as "listening", so
/// unit tests that never install it behave as before.
static INBOX_HEALTH: OnceLock<Arc<InboxHealth>> = OnceLock::new();

/// Why [`InboxHealth::install_global`] refused. Mirrors `spam_gate::InstallError`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallError {
    /// A health record is already installed.
    AlreadyInstalled,
}

/// One stretch during which the daemon could not hear.
///
/// Timestamps are wall-clock seconds, the same base as an order's `taken_at`,
/// because that is what these windows are ultimately intersected against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BlindWindow {
    start: i64,
    /// `None` while the outage is still running.
    end: Option<i64>,
}

impl BlindWindow {
    /// Seconds of this window that fall inside `[from, to]`.
    fn overlap(&self, from: i64, to: i64, now: i64) -> i64 {
        let end = self.end.unwrap_or(now);
        (end.min(to) - self.start.max(from)).max(0)
    }
}

/// Windows that ended longer ago than this are dropped: no order the timeout
/// job can still be looking at was waiting back then, so they can no longer
/// change any verdict. Generous next to `expiration_seconds` (900s by default)
/// so the bound is never the reason a user loses compensation.
const BLIND_WINDOW_RETENTION_SECS: i64 = 7 * 24 * 3600;

/// Hard cap on retained windows, so a relay flapping in a tight loop cannot
/// grow this unboundedly between prunes.
const MAX_BLIND_WINDOWS: usize = 512;

#[derive(Debug)]
struct HealthState {
    /// The last verdict an audit reached. `None` until the first one runs:
    /// startup is not evidence that the inbox works, and must not be read as
    /// such (see [`InboxHealth::is_confirmed_listening`]).
    verdict: Option<InboxStatus>,
    /// When the record was installed, which is the earliest moment an outage
    /// discovered by the first audit could have begun.
    installed_at: i64,
    /// Every outage this process has seen, oldest first, pruned by age.
    windows: Vec<BlindWindow>,
    /// Relay to the wall-clock second its last `EOSE` for the inbox arrived.
    ///
    /// A relay is only credited with serving the inbox once it says so. The
    /// SDK's own subscription map cannot stand in for this: it records what
    /// *we* sent, so a relay that holds the connection open and quietly
    /// ignores the REQ still looks subscribed there.
    ///
    /// The timestamp is what binds the credit to a single websocket session.
    /// On reconnect the SDK re-sends the REQ by itself, with no frame this
    /// module can observe, so an acknowledgement earned on the previous
    /// connection says nothing about the current one — see
    /// [`InboxHealth::has_acknowledged_since`].
    acknowledged: HashMap<RelayUrl, i64>,
    /// Re-subscribe pacing, drawn on by the event loop and the watchdog alike.
    ///
    /// Only holds relays that are currently failing; a relay that answers the
    /// REQ is dropped from the map, so the steady state is empty.
    backoff: HashMap<RelayUrl, RelayBackoff>,
}

impl HealthState {
    fn blind_now(&self) -> Option<&BlindWindow> {
        self.windows.last().filter(|w| w.end.is_none())
    }
}

/// Tracks whether the daemon's ear is open, and when it was not.
///
/// The scheduler's timeout machinery reads this: an order is only "late" if
/// Mostro was in a position to hear from the user, and the ten-second replay
/// window means a message sent into a dead inbox is gone rather than delayed
/// (see the module docs). Punishing a user for a silence the node itself
/// caused would be unfair, so the timeout clock stops while the inbox is down.
#[derive(Debug)]
pub struct InboxHealth {
    state: Mutex<HealthState>,
}

impl Default for InboxHealth {
    fn default() -> Self {
        Self::new()
    }
}

impl InboxHealth {
    pub fn new() -> Self {
        Self::at(now_secs())
    }

    /// The shared state, recovering rather than propagating a poisoned lock.
    ///
    /// [`HealthState`] is plain data with no invariant a panic could leave
    /// half-applied, and no user code runs under the guard — so poisoning
    /// carries no information worth acting on. Propagating it would, though:
    /// every consumer of this record is a maintenance job, and
    /// [`Self::is_confirmed_listening`] is read by the timeout job on every
    /// tick. A panic there kills that task for the life of the process, which
    /// stops timeouts permanently — hold invoices ride to CLTV expiry and
    /// bonds never resolve. Taking the data as it stands is strictly better
    /// than that.
    fn state(&self) -> MutexGuard<'_, HealthState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(super) fn at(installed_at: i64) -> Self {
        Self {
            state: Mutex::new(HealthState {
                verdict: None,
                installed_at,
                windows: Vec::new(),
                acknowledged: HashMap::new(),
                backoff: HashMap::new(),
            }),
        }
    }

    /// Install as the process-wide health record.
    pub fn install_global(self) -> Result<(), InstallError> {
        INBOX_HEALTH
            .set(Arc::new(self))
            .map_err(|_| InstallError::AlreadyInstalled)
    }

    /// The process-wide health record, if one was installed.
    pub fn global() -> Option<Arc<InboxHealth>> {
        INBOX_HEALTH.get().cloned()
    }

    /// Record the current observation, returning the resulting status.
    pub(super) fn observe(&self, status: InboxStatus, now: i64) -> InboxStatus {
        let mut state = self.state();
        let first_verdict = state.verdict.is_none();
        state.verdict = Some(status);

        match (status, state.blind_now().is_some()) {
            (InboxStatus::Blind, false) => {
                // A first audit that finds the inbox deaf has found an outage
                // that was already running: the node has not heard anything
                // since it came up, so that is when the outage began.
                let start = if first_verdict {
                    state.installed_at
                } else {
                    now
                };
                state.windows.push(BlindWindow { start, end: None });
            }
            (InboxStatus::Listening, true) => {
                if let Some(open) = state.windows.last_mut() {
                    open.end = Some(now);
                }
            }
            _ => {}
        }

        state.windows.retain(|w| match w.end {
            Some(end) => end > now - BLIND_WINDOW_RETENTION_SECS,
            None => true,
        });
        if state.windows.len() > MAX_BLIND_WINDOWS {
            let excess = state.windows.len() - MAX_BLIND_WINDOWS;
            state.windows.drain(..excess);
        }

        status
    }

    /// Whether the inbox is deaf right now.
    ///
    /// A record that has never been audited is not blind — but neither is it
    /// known to be listening, which is the question a caller about to act on a
    /// user's silence should be asking. See [`Self::is_confirmed_listening`].
    pub fn is_blind(&self) -> bool {
        self.state().blind_now().is_some()
    }

    /// Whether an audit has actually confirmed that Mostro can hear.
    ///
    /// This is the predicate for anything that punishes a user for not
    /// answering. It is deliberately false before the first audit: the daemon
    /// subscribes at startup and the watchdog's first pass comes later, so
    /// between the two there is a window in which a node that never obtained a
    /// working inbox would otherwise look healthy and start cancelling orders
    /// and slashing bonds over messages it was never in a position to receive.
    pub fn is_confirmed_listening(&self) -> bool {
        self.state().verdict == Some(InboxStatus::Listening)
    }

    /// Seconds the inbox was deaf between `from` and now.
    ///
    /// This is the compensation a single order is owed, and it is computed per
    /// order on purpose. A deadline is wall-clock, but the user is answering a
    /// node that has to be listening for the answer to land — and because a
    /// message sent into a dead inbox is lost rather than queued (the
    /// ten-second replay window, see the module docs), they must send it again
    /// once the ear is back. So an order's clock effectively stops for exactly
    /// the outages that overlap its own waiting period: an order that was
    /// already waiting through an outage is owed all of it, one taken
    /// afterwards is owed nothing.
    pub fn blind_seconds_since(&self, from: i64) -> i64 {
        self.blind_seconds_between(from, now_secs())
    }

    fn blind_seconds_between(&self, from: i64, to: i64) -> i64 {
        let state = self.state();
        state.windows.iter().map(|w| w.overlap(from, to, to)).sum()
    }

    /// Upper bound on what any order could be owed.
    ///
    /// No decision is taken on this figure — an order's credit is always the
    /// downtime that overlaps its own wait ([`Self::blind_seconds_since`]).
    /// It exists so the timeout job can tell an operator, in one line, how
    /// much downtime is in play this tick.
    pub fn max_blind_seconds(&self) -> i64 {
        let now = now_secs();
        let state = self.state();
        state
            .windows
            .iter()
            .map(|w| w.end.unwrap_or(now) - w.start)
            .sum::<i64>()
            .max(0)
    }

    /// Record that `relay` answered the inbox REQ (an `EOSE` for our
    /// subscription), which is the only evidence that it is really serving it.
    ///
    /// Whatever was making the relay fail is over, so its pacing is reset too
    /// and the next failure earns a prompt retry again. Returns whether the
    /// relay was being backed off, which is what distinguishes a recovery
    /// worth logging from the steady state.
    pub fn note_relay_acknowledged(&self, relay: &RelayUrl) -> bool {
        self.note_relay_acknowledged_at(relay, now_secs())
    }

    pub(super) fn note_relay_acknowledged_at(&self, relay: &RelayUrl, at: i64) -> bool {
        let mut state = self.state();
        state.acknowledged.insert(relay.clone(), at);
        state.backoff.remove(relay).is_some()
    }

    /// Whether a re-subscribe to `relay` may go out now, arming the next delay
    /// when it may. The first failure for a relay always passes.
    ///
    /// This is the single pacing budget the event loop and the watchdog share.
    /// Under the watchdog's 30-second cadence the doubling only starts to bite
    /// once the delay outgrows the interval — so a relay that lost the inbox
    /// once is re-subscribed on the very next pass, and only one that keeps
    /// refusing tapers to [`RESUBSCRIBE_MAX_BACKOFF`].
    pub fn allow_resubscribe(&self, relay: &RelayUrl) -> bool {
        self.allow_resubscribe_at(relay, Instant::now())
    }

    fn allow_resubscribe_at(&self, relay: &RelayUrl, now: Instant) -> bool {
        let mut state = self.state();
        match state.backoff.get_mut(relay) {
            None => {
                state.backoff.insert(
                    relay.clone(),
                    RelayBackoff {
                        next_attempt_at: now + RESUBSCRIBE_INITIAL_BACKOFF,
                        delay: RESUBSCRIBE_INITIAL_BACKOFF,
                    },
                );
                true
            }
            Some(pacing) => {
                if now < pacing.next_attempt_at {
                    return false;
                }
                pacing.delay = (pacing.delay * 2).min(RESUBSCRIBE_MAX_BACKOFF);
                pacing.next_attempt_at = now + pacing.delay;
                true
            }
        }
    }

    /// Forget `relay`'s acknowledgement: whatever it said about serving the
    /// inbox no longer applies.
    ///
    /// Two callers, one meaning. A fresh REQ has gone out and has yet to be
    /// answered (`resubscribe_relay`); or the relay closed the subscription
    /// provisionally, so the entry the SDK left registered is not evidence of
    /// anything until the replacement REQ is answered
    /// (`InboxKeeper::on_relay_message`). In both cases the audit has to judge
    /// the relay on the new evidence rather than on the old `EOSE`.
    pub fn note_relay_unacknowledged(&self, relay: &RelayUrl) {
        self.state().acknowledged.remove(relay);
    }

    /// Whether `relay` answered the inbox REQ *on its current connection*.
    ///
    /// `connected_at` is when the websocket the audit is looking at was
    /// established. An acknowledgement older than that was earned on a session
    /// that no longer exists: the SDK re-sends the REQ on reconnect of its own
    /// accord (`should_resubscribe`), emitting nothing this module can see, so
    /// a relay that comes back and then quietly ignores the replacement would
    /// otherwise keep reading as healthy on the strength of its old `EOSE`.
    ///
    /// The comparison is inclusive so that an `EOSE` landing in the same
    /// second as the connect still counts.
    pub fn has_acknowledged_since(&self, relay: &RelayUrl, connected_at: i64) -> bool {
        self.state()
            .acknowledged
            .get(relay)
            .is_some_and(|&at| at >= connected_at)
    }

    /// How long the current outage has been running, or zero if listening.
    pub fn blind_for_secs(&self) -> i64 {
        let now = now_secs();
        self.state()
            .blind_now()
            .map(|w| (now - w.start).max(0))
            .unwrap_or(0)
    }

    /// How long it has been since an audit confirmed Mostro can hear.
    ///
    /// Zero while listening. Otherwise it counts from the start of the current
    /// outage, or — if no audit has ever run — from startup, so a watchdog
    /// that never reported cannot leave a caller waiting forever on a verdict
    /// that is not coming.
    pub fn unconfirmed_for_secs(&self) -> i64 {
        let now = now_secs();
        let state = self.state();
        if state.verdict == Some(InboxStatus::Listening) && state.blind_now().is_none() {
            return 0;
        }
        let since = state
            .blind_now()
            .map(|w| w.start)
            .unwrap_or(state.installed_at);
        (now - since).max(0)
    }

    /// Whether re-subscribes to `relay` are currently being paced.
    ///
    /// Pacing is deliberately not observable in production — callers ask
    /// [`Self::allow_resubscribe`], which also arms the next delay — but the
    /// keeper's tests next door assert on it, and the map is private here.
    #[cfg(test)]
    pub(super) fn is_backing_off(&self, relay: &RelayUrl) -> bool {
        self.state().backoff.contains_key(relay)
    }
}

/// Wall-clock seconds, the base an order's `taken_at` is recorded in.
pub(super) fn now_secs() -> i64 {
    Timestamp::now().as_secs() as i64
}

/// Origin for the wall-clock arithmetic under test: health observations are
/// timestamp-based, so tests drive a fixed origin rather than the real clock.
/// Shared with the keeper's tests next door.
#[cfg(test)]
pub(super) const T0: i64 = 1_700_000_000;

#[cfg(test)]
mod tests {
    use super::*;

    fn relay_url(url: &str) -> RelayUrl {
        RelayUrl::parse(url).expect("valid relay url")
    }

    // ───────────────────────────── backoff pacing ─────────────────────────────

    #[test]
    fn first_closure_from_a_relay_retries_immediately() {
        let health = InboxHealth::at(T0);
        let relay = relay_url("ws://relay.example");

        assert!(
            health.allow_resubscribe_at(&relay, Instant::now()),
            "a first CLOSED must be answered at once: every delay is deaf time"
        );
    }

    #[test]
    fn repeat_closures_are_paced_and_back_off() {
        let health = InboxHealth::at(T0);
        let relay = relay_url("ws://relay.example");
        let start = Instant::now();

        assert!(health.allow_resubscribe_at(&relay, start));
        // A relay that closes again right away must not pull a second REQ.
        assert!(!health.allow_resubscribe_at(&relay, start));
        assert!(!health.allow_resubscribe_at(&relay, start + Duration::from_secs(1)));

        // Past the first delay it retries, and the next wait is longer.
        assert!(health.allow_resubscribe_at(&relay, start + RESUBSCRIBE_INITIAL_BACKOFF));
        assert!(!health.allow_resubscribe_at(&relay, start + RESUBSCRIBE_INITIAL_BACKOFF * 2));
        assert!(health.allow_resubscribe_at(&relay, start + RESUBSCRIBE_INITIAL_BACKOFF * 3));
    }

    #[test]
    fn backoff_is_capped() {
        let health = InboxHealth::at(T0);
        let relay = relay_url("ws://relay.example");
        let mut now = Instant::now();

        // Drive it well past the ceiling.
        for _ in 0..20 {
            assert!(health.allow_resubscribe_at(&relay, now));
            now += RESUBSCRIBE_MAX_BACKOFF * 2;
        }

        assert_eq!(
            health
                .state
                .lock()
                .unwrap()
                .backoff
                .get(&relay)
                .expect("state kept")
                .delay,
            RESUBSCRIBE_MAX_BACKOFF,
            "a hostile relay must still be retried every {RESUBSCRIBE_MAX_BACKOFF:?}"
        );
    }

    #[test]
    fn backoff_is_per_relay() {
        let health = InboxHealth::at(T0);
        let hostile = relay_url("ws://hostile.example");
        let healthy = relay_url("ws://healthy.example");
        let now = Instant::now();

        assert!(health.allow_resubscribe_at(&hostile, now));
        assert!(!health.allow_resubscribe_at(&hostile, now));
        // One misbehaving relay must not delay recovery on another.
        assert!(health.allow_resubscribe_at(&healthy, now));
    }

    #[test]
    fn an_acknowledgement_clears_the_pacing_for_the_next_failure() {
        let health = InboxHealth::at(T0);
        let relay = relay_url("ws://relay.example");
        let now = Instant::now();

        assert!(health.allow_resubscribe_at(&relay, now));
        assert!(!health.allow_resubscribe_at(&relay, now));

        assert!(
            health.note_relay_acknowledged(&relay),
            "clearing a live backoff entry is what marks a recovery"
        );
        assert!(
            health.allow_resubscribe_at(&relay, now),
            "a relay that answered starts over: the next failure is a fresh one"
        );

        // The steady state has nothing to clear, so nothing to report either.
        health.note_relay_acknowledged(&relay);
        assert!(!health.note_relay_acknowledged(&relay));
    }

    #[test]
    fn the_watchdog_cadence_recovers_promptly_and_only_then_tapers() {
        // The point of sharing one budget: an audit every
        // `INBOX_WATCHDOG_INTERVAL` must still re-subscribe a relay that
        // simply lost the inbox, while a relay that refuses it converges on
        // the advertised ceiling instead of drawing a REQ every 30 seconds
        // forever.
        let health = InboxHealth::at(T0);
        let relay = relay_url("ws://hostile.example");
        let tick = Duration::from_secs(crate::scheduler::INBOX_WATCHDOG_INTERVAL);
        let mut now = Instant::now();

        assert!(
            health.allow_resubscribe_at(&relay, now),
            "the pass that first notices the loss must act on it"
        );
        for pass in 1..=3 {
            now += tick;
            assert!(
                health.allow_resubscribe_at(&relay, now),
                "pass {pass}: a delay still under the audit interval must not skip a retry"
            );
        }

        // Once the doubling outgrows the interval, passes start being skipped.
        let mut attempts = 0;
        for _ in 0..40 {
            now += tick;
            if health.allow_resubscribe_at(&relay, now) {
                attempts += 1;
            }
        }
        assert!(
            attempts < 40,
            "a relay that keeps refusing must stop drawing a REQ on every pass"
        );
        assert_eq!(
            health
                .state
                .lock()
                .unwrap()
                .backoff
                .get(&relay)
                .expect("state kept")
                .delay,
            RESUBSCRIBE_MAX_BACKOFF
        );
    }

    // ───────────────────────────── health record ─────────────────────────────

    #[test]
    fn health_records_an_outage_from_first_blindness_to_recovery() {
        let health = InboxHealth::at(T0);

        assert!(!health.is_blind(), "a fresh record starts out listening");

        health.observe(InboxStatus::Blind, T0);
        assert!(health.is_blind());

        // Staying blind must not restart the clock — the outage began at the
        // first observation, and that is what an order is owed.
        health.observe(InboxStatus::Blind, T0 + 30);
        assert!(health.is_blind());

        health.observe(InboxStatus::Listening, T0 + 90);
        assert!(!health.is_blind());

        assert_eq!(
            health.blind_seconds_between(T0, T0 + 90),
            90,
            "the recorded outage must span the whole blind window"
        );
    }

    #[test]
    fn health_is_not_listening_until_an_audit_says_so() {
        let health = InboxHealth::at(T0);

        // Startup is not evidence. Between `main` subscribing and the
        // watchdog's first pass, a node whose inbox never worked would
        // otherwise process timeouts as if it had been listening all along.
        assert!(
            !health.is_confirmed_listening(),
            "an unaudited record must not authorise acting on a user's silence"
        );
        assert!(
            !health.is_blind(),
            "nor should it claim an outage it has not observed"
        );

        health.observe(InboxStatus::Listening, T0);
        assert!(health.is_confirmed_listening());
    }

    #[test]
    fn a_blind_first_audit_dates_the_outage_from_startup() {
        let health = InboxHealth::at(T0);

        // The watchdog's first pass comes some time after boot. Finding the
        // inbox deaf then means it was deaf for that whole stretch, not just
        // from the moment somebody looked.
        health.observe(InboxStatus::Blind, T0 + 30);
        health.observe(InboxStatus::Listening, T0 + 90);

        assert_eq!(
            health.blind_seconds_between(T0, T0 + 90),
            90,
            "the outage must be dated from startup, not from the first audit"
        );
    }

    #[test]
    fn a_node_that_was_never_blind_owes_nothing() {
        let health = InboxHealth::at(T0);
        health.observe(InboxStatus::Listening, T0);

        assert_eq!(health.blind_seconds_between(T0, T0 + 10_000), 0);
        assert_eq!(health.max_blind_seconds(), 0);
    }

    // ──────────────────── what a single order is owed ────────────────────

    #[test]
    fn an_order_is_owed_only_the_downtime_it_waited_through() {
        let health = InboxHealth::at(T0);
        // One outage: [T0+100, T0+400], five minutes.
        health.observe(InboxStatus::Listening, T0);
        health.observe(InboxStatus::Blind, T0 + 100);
        health.observe(InboxStatus::Listening, T0 + 400);

        let now = T0 + 1_000;

        // Waiting since before it started: owed the whole outage.
        assert_eq!(health.blind_seconds_between(T0, now), 300);
        // Taken midway through: owed only the remainder.
        assert_eq!(health.blind_seconds_between(T0 + 250, now), 150);
        // Taken after it ended: owed nothing. This is what a single global
        // allowance got wrong — it credited orders that never lost a second.
        assert_eq!(health.blind_seconds_between(T0 + 500, now), 0);
    }

    #[test]
    fn compensation_does_not_evaporate_as_time_passes() {
        let health = InboxHealth::at(T0);
        health.observe(InboxStatus::Listening, T0);
        health.observe(InboxStatus::Blind, T0 + 100);
        health.observe(InboxStatus::Listening, T0 + 400);

        // The debt an order carries is a property of when it waited, not of
        // how long ago the outage was. A decaying allowance wore off at the
        // same rate the deadline advanced, so it compensated almost nothing.
        for probe in [400, 700, 5_000, 50_000] {
            assert_eq!(
                health.blind_seconds_between(T0, T0 + probe),
                300,
                "an order waiting since T0 is owed the outage regardless of when we ask"
            );
        }
    }

    #[test]
    fn an_order_waiting_through_an_outage_survives_its_nominal_deadline() {
        // The regression in full: 900s timeout, an order taken at T0, and a
        // 300s outage right at the start. Under the old decaying allowance
        // this order was cancelled at ~T0+900, having had only 600s of
        // listening time.
        let health = InboxHealth::at(T0);
        health.observe(InboxStatus::Blind, T0);
        health.observe(InboxStatus::Listening, T0 + 300);

        let exp_seconds = 900i64;
        let late_at = |now: i64| {
            let owed = health.blind_seconds_between(T0, now);
            (now - T0) >= exp_seconds + owed
        };

        assert!(!late_at(T0 + 900), "cancelled after only 600s of listening");
        assert!(!late_at(T0 + 1_199));
        assert!(
            late_at(T0 + 1_200),
            "and it must still expire once it has had its full 900s"
        );
    }

    /// Regression: the credit is per order, so an order taken *after* an
    /// outage ended must expire at its nominal deadline.
    ///
    /// The timeout job used to widen `find_order_by_seconds` by
    /// [`InboxHealth::max_blind_seconds`], which narrows the selection rather
    /// than widening it — every surviving row was already past
    /// `deadline + max_blind_seconds`, the per-order check could never spare
    /// anything, and what shipped was the global allowance this design
    /// rejects. That allowance grows with every outage in the retention
    /// window, so a node with flapping relays would postpone every deadline by
    /// hours of unrelated downtime.
    #[test]
    fn an_order_taken_after_an_outage_is_not_credited_for_it() {
        let health = InboxHealth::at(T0);
        // One outage: [T0, T0+300].
        health.observe(InboxStatus::Blind, T0);
        health.observe(InboxStatus::Listening, T0 + 300);

        let exp_seconds = 900i64;
        let late_at = |taken_at: i64, now: i64| {
            let owed = health.blind_seconds_between(taken_at, now);
            (now - taken_at) >= exp_seconds + owed
        };

        // A: waited through the whole outage, owed all 300s.
        assert!(!late_at(T0, T0 + 1_199));
        assert!(late_at(T0, T0 + 1_200));

        // B: taken after recovery, owed nothing — even though the node's total
        // downtime is the same 300s the global allowance would have handed it.
        assert_eq!(health.max_blind_seconds(), 300);
        assert!(!late_at(T0 + 400, T0 + 1_299));
        assert!(
            late_at(T0 + 400, T0 + 1_300),
            "an order that never lost a second must expire at its nominal deadline"
        );
    }

    #[test]
    fn consecutive_outages_accumulate_their_debt() {
        let health = InboxHealth::at(T0);
        health.observe(InboxStatus::Listening, T0);
        health.observe(InboxStatus::Blind, T0 + 100);
        health.observe(InboxStatus::Listening, T0 + 200);
        health.observe(InboxStatus::Blind, T0 + 240);
        health.observe(InboxStatus::Listening, T0 + 290);

        assert_eq!(
            health.blind_seconds_between(T0, T0 + 1_000),
            150,
            "an order waiting through both outages is owed both"
        );
        assert_eq!(
            health.blind_seconds_between(T0 + 210, T0 + 1_000),
            50,
            "one taken between them is owed only the second"
        );
    }

    #[test]
    fn an_ongoing_outage_counts_up_to_now() {
        let health = InboxHealth::at(T0);
        health.observe(InboxStatus::Listening, T0);
        health.observe(InboxStatus::Blind, T0 + 100);

        assert_eq!(health.blind_seconds_between(T0, T0 + 400), 300);
        assert_eq!(health.blind_seconds_between(T0, T0 + 900), 800);
    }

    #[test]
    fn stale_windows_are_pruned() {
        let health = InboxHealth::at(T0);
        health.observe(InboxStatus::Listening, T0);
        health.observe(InboxStatus::Blind, T0 + 100);
        health.observe(InboxStatus::Listening, T0 + 200);

        // Far past the retention horizon, the old window is dropped rather
        // than accumulating for the life of the process.
        let much_later = T0 + BLIND_WINDOW_RETENTION_SECS + 1_000;
        health.observe(InboxStatus::Listening, much_later);

        assert_eq!(health.blind_seconds_between(T0, much_later), 0);
        assert!(health.state.lock().expect("lock").windows.is_empty());
    }

    #[test]
    fn unconfirmed_time_counts_from_the_outage_or_from_startup() {
        // What bounds how long the timeout job may defer. It has to answer
        // even when no audit ever ran, or a watchdog that died would park the
        // job on a verdict that is never coming.
        let never_audited = InboxHealth::at(now_secs() - 120);
        assert!(
            never_audited.unconfirmed_for_secs() >= 120,
            "with no verdict at all, the clock runs from startup"
        );

        let healthy = InboxHealth::at(now_secs());
        healthy.observe(InboxStatus::Listening, now_secs());
        assert_eq!(
            healthy.unconfirmed_for_secs(),
            0,
            "a confirmed inbox owes no waiting"
        );

        let blind = InboxHealth::at(now_secs() - 600);
        blind.observe(InboxStatus::Listening, now_secs() - 600);
        blind.observe(InboxStatus::Blind, now_secs() - 300);
        assert!(
            (300..=310).contains(&blind.unconfirmed_for_secs()),
            "while blind it runs from the start of the outage, got {}",
            blind.unconfirmed_for_secs()
        );
    }

    #[test]
    fn health_ignores_repeated_healthy_observations() {
        let health = InboxHealth::at(T0);

        health.observe(InboxStatus::Listening, T0);
        health.observe(InboxStatus::Listening, T0 + 30);

        assert!(!health.is_blind());
        assert_eq!(
            health.blind_seconds_between(T0, T0 + 30),
            0,
            "a node that was never blind has no outage to compensate for"
        );
    }

    #[test]
    fn an_acknowledgement_does_not_survive_the_connection_it_was_earned_on() {
        // A websocket drop and reconnect leaves no trace the keeper can act
        // on: there is no relay-status `ClientNotification` in nostr-sdk
        // 0.45.1, and the SDK silently re-sends the REQ by itself
        // (`should_resubscribe`). If the relay then ignores that replacement,
        // the only thing standing between a deaf node and resumed slashing is
        // the acknowledgement expiring with its session.
        let health = InboxHealth::at(T0);
        let url = relay_url("ws://relay.example");

        health.note_relay_acknowledged_at(&url, T0 + 100);

        assert!(health.has_acknowledged_since(&url, T0 + 50));
        assert!(
            health.has_acknowledged_since(&url, T0 + 100),
            "an EOSE landing in the same second as the connect must still count"
        );
        assert!(
            !health.has_acknowledged_since(&url, T0 + 101),
            "credit earned on a previous connection must not vouch for this one"
        );
    }
}
