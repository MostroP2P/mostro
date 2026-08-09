//! Protocol-v2 anti-spam gate (spec §6 Phase 2, docs/TRANSPORT_V2_SPEC.md).
//!
//! The whole point of the kind-14 transport is that the visible sender is the
//! authoring **trade key**, so the daemon can pre-validate cheaply *before*
//! paying the NIP-44 decrypt cost. This module holds the two in-memory pieces
//! that make that work:
//!
//! 1. **Active-trade-pubkey cache** — the set of trade keys that legitimately
//!    message Mostro right now (participants of non-terminal orders + active
//!    dispute solvers). Rebuilt periodically from the DB by a scheduler job
//!    (`job_refresh_active_pubkeys`) and warmed once at startup.
//! 2. **Replay guard** — a short-window dedup of seen event ids, so a flood of
//!    re-sent identical events is dropped before decryption (defense in depth).
//!
//! The gate has **two lanes**: a *known-keys lane* (sender in the cache →
//! fast-path, only the base `pow` applies) and a *first-contact lane* (sender
//! unseen → must clear the stiffer `pow_first_contact` before the daemon
//! decrypts). Brand-new orders and takes legitimately arrive on the
//! first-contact lane; that is also where spam concentrates, so PoW (plus
//! relay-side rate limiting) is the toll there.
//!
//! Only the v2 (`nip44`) transport uses this gate — v1 gift wraps are authored
//! by throwaway keys that carry no pre-validatable signal.
//!
//! Follows the established global-singleton pattern (`OnceLock`, like
//! `PRICE_MANAGER` / `MOSTRO_CONFIG`); the cache is an inner `RwLock` and the
//! replay guard a `Mutex`, matching the daemon's single-consumer event loop.

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock, RwLock};

use nostr_sdk::EventId;

/// How long (seconds) a seen event id is remembered for replay dedup. Must be
/// ≥ the event loop's 10-second freshness window so a duplicate can never slip
/// past the guard while the original is still acceptable; 60s adds margin for
/// clock skew without unbounded memory (entries are pruned past this age).
pub const REPLAY_WINDOW_SECS: i64 = 60;

/// Process-wide gate. `None` until [`SpamGate::install_global`] runs in `main`;
/// the event loop treats an absent gate as fail-open (no pre-filtering), so
/// unit tests that never install it are unaffected.
static SPAM_GATE: OnceLock<SpamGate> = OnceLock::new();

/// Why [`SpamGate::install_global`] refused. A zero-size enum (not the bulky
/// `SpamGate`) so the `Result` stays small; mirrors `price::InstallError`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallError {
    /// A gate is already installed.
    AlreadyInstalled,
}

/// Short-window dedup of event ids (defense in depth, §6 Phase 2).
struct ReplayGuard {
    seen: HashMap<EventId, i64>,
    window_secs: i64,
}

impl ReplayGuard {
    fn new(window_secs: i64) -> Self {
        Self {
            seen: HashMap::new(),
            window_secs,
        }
    }

    /// Record `id` as seen at `now` and report whether it was **already**
    /// present within the window (i.e. a replay the caller should drop).
    /// Expired entries are pruned on the way through so the map stays bounded
    /// by the in-window event rate.
    fn check_and_record(&mut self, id: EventId, now: i64) -> bool {
        let cutoff = now - self.window_secs;
        self.seen.retain(|_, &mut seen_at| seen_at >= cutoff);
        // `insert` returns the previous value if the key was present — a
        // non-expired prior sighting means this is a replay.
        self.seen.insert(id, now).is_some()
    }
}

/// The anti-spam gate: known-keys cache + replay guard.
pub struct SpamGate {
    known: RwLock<HashSet<String>>,
    replay: Mutex<ReplayGuard>,
}

impl SpamGate {
    /// Build an empty gate with the given replay window.
    pub fn new(replay_window_secs: i64) -> Self {
        Self {
            known: RwLock::new(HashSet::new()),
            replay: Mutex::new(ReplayGuard::new(replay_window_secs)),
        }
    }

    /// Install as the process-wide gate. Mirrors `PriceManager::install_global`:
    /// a second call returns `Err(AlreadyInstalled)` rather than panicking.
    /// (The large `SpamGate` is dropped on the error path rather than returned,
    /// keeping the `Result` small — clippy `result_large_err`.)
    pub fn install_global(self) -> Result<(), InstallError> {
        SPAM_GATE
            .set(self)
            .map_err(|_| InstallError::AlreadyInstalled)
    }

    /// Borrow the installed gate, if any. `None` ⇒ not installed (fail-open).
    pub fn global() -> Option<&'static SpamGate> {
        SPAM_GATE.get()
    }

    /// Replace the known-keys set wholesale with the latest snapshot from the
    /// DB. A poisoned lock is logged and skipped — a stale cache only costs a
    /// few legitimate keys a trip through the first-contact lane, never a
    /// crash.
    pub fn set_known<I: IntoIterator<Item = String>>(&self, keys: I) {
        match self.known.write() {
            Ok(mut set) => {
                *set = keys.into_iter().collect();
            }
            Err(_) => tracing::error!("spam_gate: known-keys lock poisoned; skipping refresh"),
        }
    }

    /// Is `pubkey` (hex trade key) a currently-active participant? A poisoned
    /// lock degrades to `false` (treat as first-contact) — the safe direction:
    /// the sender just pays the PoW toll instead of being waved through.
    pub fn is_known(&self, pubkey: &str) -> bool {
        self.known
            .read()
            .map(|set| set.contains(pubkey))
            .unwrap_or(false)
    }

    /// Number of cached active keys (diagnostics / tests).
    pub fn known_count(&self) -> usize {
        self.known.read().map(|set| set.len()).unwrap_or(0)
    }

    /// Record `id` and report whether it is a replay to drop. A poisoned lock
    /// degrades to `false` (never drop a real message because dedup state was
    /// lost).
    pub fn is_replay(&self, id: EventId, now: i64) -> bool {
        match self.replay.lock() {
            Ok(mut guard) => guard.check_and_record(id, now),
            Err(_) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr_sdk::{EventBuilder, Keys};

    fn an_event_id(note: &str) -> EventId {
        EventBuilder::text_note(note)
            .sign_with_keys(&Keys::generate())
            .expect("sign test event")
            .id
    }

    #[test]
    fn known_set_membership_and_replace() {
        let gate = SpamGate::new(REPLAY_WINDOW_SECS);
        assert!(!gate.is_known("a"));
        gate.set_known(["a".to_string(), "b".to_string()]);
        assert!(gate.is_known("a"));
        assert!(gate.is_known("b"));
        assert!(!gate.is_known("c"));
        assert_eq!(gate.known_count(), 2);

        // set_known replaces wholesale (a refreshed DB snapshot, not a merge):
        // a key no longer active drops out.
        gate.set_known(["c".to_string()]);
        assert!(gate.is_known("c"));
        assert!(!gate.is_known("a"));
        assert_eq!(gate.known_count(), 1);
    }

    #[test]
    fn replay_first_seen_then_duplicate() {
        let gate = SpamGate::new(REPLAY_WINDOW_SECS);
        let id = an_event_id("dup");
        let now = 1_000_000;
        assert!(!gate.is_replay(id, now), "first sighting is not a replay");
        assert!(
            gate.is_replay(id, now + 1),
            "second sighting within window is a replay"
        );
        assert!(
            gate.is_replay(id, now + 30),
            "still a replay later in the window"
        );
    }

    #[test]
    fn distinct_ids_are_independent() {
        let gate = SpamGate::new(REPLAY_WINDOW_SECS);
        let a = an_event_id("a");
        let b = an_event_id("b");
        let now = 500;
        assert!(!gate.is_replay(a, now));
        assert!(!gate.is_replay(b, now), "a different id is not a replay");
        assert!(gate.is_replay(a, now), "but re-seeing a is");
    }

    #[test]
    fn entry_expires_after_window() {
        let mut guard = ReplayGuard::new(60);
        let id = an_event_id("expire");
        assert!(!guard.check_and_record(id, 1_000));
        // Past the window the prior sighting is pruned, so it reads as fresh.
        assert!(!guard.check_and_record(id, 1_000 + 61));
        // ...and is tracked again from the new timestamp.
        assert!(guard.check_and_record(id, 1_000 + 61));
    }

    #[test]
    fn install_global_then_second_install_is_rejected() {
        // The OnceLock is process-wide, so this single test owns both the
        // first (successful) install and the AlreadyInstalled rejection.
        let first = SpamGate::new(REPLAY_WINDOW_SECS).install_global();
        assert!(first.is_ok(), "first install must succeed");
        assert!(
            SpamGate::global().is_some(),
            "global() must expose the installed gate"
        );

        let second = SpamGate::new(REPLAY_WINDOW_SECS).install_global();
        assert_eq!(
            second,
            Err(InstallError::AlreadyInstalled),
            "a second install must be refused, not panic"
        );
    }

    #[test]
    fn poisoned_known_lock_degrades_to_first_contact_lane() {
        let gate = std::sync::Arc::new(SpamGate::new(REPLAY_WINDOW_SECS));
        gate.set_known(["known".to_string()]);
        assert!(gate.is_known("known"));

        // Poison the known-keys RwLock by panicking while holding the writer.
        let poisoner = std::sync::Arc::clone(&gate);
        let _ = std::thread::spawn(move || {
            let _guard = poisoner.known.write().unwrap();
            panic!("poison known lock");
        })
        .join();

        // Degradation contract: reads fall back to "unknown" (safe direction),
        // counts to 0, and refresh is skipped without crashing.
        assert!(!gate.is_known("known"), "poisoned read degrades to false");
        assert_eq!(gate.known_count(), 0, "poisoned count degrades to 0");
        gate.set_known(["other".to_string()]); // must log-and-skip, not panic
        assert!(!gate.is_known("other"));
    }

    #[test]
    fn poisoned_replay_lock_never_drops_messages() {
        let gate = std::sync::Arc::new(SpamGate::new(REPLAY_WINDOW_SECS));
        let id = an_event_id("poisoned-replay");
        assert!(!gate.is_replay(id, 1_000));

        // Poison the replay Mutex by panicking while holding the guard.
        let poisoner = std::sync::Arc::clone(&gate);
        let _ = std::thread::spawn(move || {
            let _guard = poisoner.replay.lock().unwrap();
            panic!("poison replay lock");
        })
        .join();

        // Even a genuine duplicate must NOT be dropped once dedup state is
        // lost — fail-open is the documented safe direction.
        assert!(
            !gate.is_replay(id, 1_001),
            "poisoned replay guard must degrade to 'not a replay'"
        );
    }

    #[test]
    fn prune_keeps_map_bounded_to_window() {
        let mut guard = ReplayGuard::new(60);
        for i in 0..100 {
            // Each a distinct id at a distinct, steadily-advancing time.
            guard.check_and_record(an_event_id(&format!("e{i}")), 10_000 + i);
        }
        // Advancing well past the window and touching the guard prunes every
        // stale entry, leaving only the one just recorded.
        let fresh = an_event_id("fresh");
        guard.check_and_record(fresh, 10_000 + 100 + 61);
        assert_eq!(
            guard.seen.len(),
            1,
            "stale entries must be pruned, leaving only the fresh sighting"
        );
    }
}
