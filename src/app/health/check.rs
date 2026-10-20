//! Checks and status, as the health event reports them (protocol:
//! `health_event.md`, "Checks" and "Deriving `status`"). Pure functions, so
//! every rule is under test without a node, a relay or a clock.

use std::collections::HashMap;
use std::fmt;

/// State of one health check.
///
/// `Unknown` means the check could not be evaluated. It never means
/// "probably fine".
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CheckState {
    Ok,
    Warn,
    Fail,
    Unknown,
}

impl CheckState {
    /// The value used in the event's `check` tags.
    pub fn as_str(self) -> &'static str {
        match self {
            CheckState::Ok => "ok",
            CheckState::Warn => "warn",
            CheckState::Fail => "fail",
            CheckState::Unknown => "unknown",
        }
    }
}

impl fmt::Display for CheckState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The closed list of checks of version 1 of the event.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum CheckName {
    LndSync,
    Payouts,
    /// Cashu instances only.
    CashuMint,
    Relays,
    Database,
}

impl CheckName {
    pub fn as_str(self) -> &'static str {
        match self {
            CheckName::LndSync => "lnd_sync",
            CheckName::Payouts => "payouts",
            CheckName::CashuMint => "cashu_mint",
            CheckName::Relays => "relays",
            CheckName::Database => "database",
        }
    }
}

/// The instance's overall state (`status` tag).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HealthStatus {
    Ok,
    Degraded,
    Down,
}

impl HealthStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            HealthStatus::Ok => "ok",
            HealthStatus::Degraded => "degraded",
            HealthStatus::Down => "down",
        }
    }
}

/// A settled order older than this (seconds) with no successful payout in
/// as long makes `payouts` fail.
pub const PAYOUTS_STUCK_FAIL_SECS: i64 = 7_200;
/// A settled order older than this (seconds) makes `payouts` warn. Half an
/// hour, not less: a buyer whose wallet is offline keeps the daemon retrying
/// for a while, and that alone does not make the node degraded.
pub const PAYOUTS_STUCK_WARN_SECS: i64 = 1_800;
/// Final attempts in the last hour needed before the failure rate counts.
pub const PAYOUTS_MIN_ATTEMPTS_FOR_RATE: u64 = 3;
/// Distinct orders with a failed attempt in the last hour needed before the
/// failure rate counts: one order retried against one bad invoice is that
/// buyer's problem, not the node's. An `insufficient_balance` failure is the
/// node's whatever the invoice, so with one the rate counts on one order.
pub const PAYOUTS_MIN_FAILING_ORDERS_FOR_RATE: u64 = 2;
/// A settled unpaid order older than this (seconds), never attempted and
/// with no payout in flight, is stale: nothing in the daemon will pay it
/// (settled on a previous node, or left by an admin settle that failed
/// before paying). It keeps `payouts` at `warn` for the operator to clear
/// it, but never makes it fail.
pub const PAYOUTS_STALE_SECS: i64 = 7 * 86_400;

/// What the `payouts` check is computed from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PayoutFacts {
    pub succeeded_1h: u64,
    pub failed_1h: u64,
    /// Distinct orders among `failed_1h`.
    pub failed_orders_1h: u64,
    /// Among `failed_1h`, those LND gave up on for `insufficient_balance`:
    /// the node's own liquidity, whichever invoice it was paying.
    pub insufficient_balance_1h: u64,
    pub last_success_at: Option<i64>,
    /// Age of the oldest settled unpaid order that is not stale, if any.
    pub oldest_settled_unpaid_age_secs: Option<i64>,
    /// Settled unpaid orders that are stale (see [`PAYOUTS_STALE_SECS`]).
    pub stale_settled_unpaid: u64,
}

/// The `payouts` check. `facts` is `None` when they could not be read (for
/// example the database query failed): the check is then `unknown`.
pub fn payouts_check(facts: Option<&PayoutFacts>, now: i64) -> CheckState {
    let Some(facts) = facts else {
        return CheckState::Unknown;
    };
    let oldest = facts.oldest_settled_unpaid_age_secs.unwrap_or(0);
    let no_recent_success = facts
        .last_success_at
        .is_none_or(|at| now.saturating_sub(at) > PAYOUTS_STUCK_FAIL_SECS);
    let stuck = oldest > PAYOUTS_STUCK_FAIL_SECS && no_recent_success;
    let attempts = facts.succeeded_1h + facts.failed_1h;
    let node_side = facts.failed_orders_1h >= PAYOUTS_MIN_FAILING_ORDERS_FOR_RATE
        || facts.insufficient_balance_1h > 0;
    let mostly_failing =
        attempts >= PAYOUTS_MIN_ATTEMPTS_FOR_RATE && node_side && facts.failed_1h * 2 > attempts;
    if stuck || mostly_failing {
        CheckState::Fail
    } else if oldest > PAYOUTS_STUCK_WARN_SECS || facts.stale_settled_unpaid > 0 {
        CheckState::Warn
    } else {
        CheckState::Ok
    }
}

/// How long some configured relay may stay disconnected, while another is
/// connected, before `relays` warns (seconds). Relays drop and reconnect on
/// their own; the event still goes out through the others meanwhile.
pub const RELAYS_MISSING_WARN_AFTER_SECS: i64 = 300;

/// When the current uninterrupted spell with some configured relay
/// disconnected began, given when it began at the previous evaluation. Only
/// every relay connected ends it, so losing all of them in the middle never
/// restarts the clock.
pub fn relays_missing_since(
    previous: Option<i64>,
    configured: usize,
    connected: usize,
    now: i64,
) -> Option<i64> {
    (connected < configured).then(|| previous.unwrap_or(now))
}

/// The `relays` check over the relays the daemon is configured to use.
/// `missing_since` is [`relays_missing_since`] at `now`.
pub fn relays_check(
    configured: usize,
    connected: usize,
    missing_since: Option<i64>,
    now: i64,
) -> CheckState {
    // A spell nobody timed is not read as a short one.
    let missing_long = missing_since
        .is_none_or(|since| now.saturating_sub(since) >= RELAYS_MISSING_WARN_AFTER_SECS);
    if connected == 0 {
        CheckState::Fail
    } else if connected >= configured || !missing_long {
        CheckState::Ok
    } else {
        CheckState::Warn
    }
}

/// The `database` check: whether a trivial query answered in time.
pub fn database_check(answered: bool) -> CheckState {
    if answered {
        CheckState::Ok
    } else {
        CheckState::Fail
    }
}

/// Derive `status` from the checks and the maintenance flag, in the order
/// the protocol gives.
pub fn derive_status(checks: &[(CheckName, CheckState)], maintenance: bool) -> HealthStatus {
    let state_of = |name: CheckName| {
        checks
            .iter()
            .find(|(check, _)| *check == name)
            .map(|(_, state)| *state)
    };
    let down = matches!(
        state_of(CheckName::LndSync),
        Some(CheckState::Fail | CheckState::Unknown)
    ) || state_of(CheckName::CashuMint) == Some(CheckState::Fail)
        || state_of(CheckName::Database) == Some(CheckState::Fail)
        || state_of(CheckName::Relays) == Some(CheckState::Fail);
    if down {
        return HealthStatus::Down;
    }
    let attention = checks.iter().any(|(_, state)| *state != CheckState::Ok);
    if attention || maintenance {
        HealthStatus::Degraded
    } else {
        HealthStatus::Ok
    }
}

/// Remembers when each check entered its current state, for the `since`
/// element of the `check` tags. After a restart a state is dated from the
/// moment it was first observed, as the protocol allows.
#[derive(Clone, Debug, Default)]
pub struct CheckTracker {
    states: HashMap<CheckName, (CheckState, i64)>,
}

impl CheckTracker {
    /// Record `state` for `name` at `now`; returns its `since`.
    pub fn update(&mut self, name: CheckName, state: CheckState, now: i64) -> i64 {
        match self.states.get(&name) {
            Some((previous, since)) if *previous == state => *since,
            _ => {
                self.states.insert(name, (state, now));
                now
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_792_000_000;

    #[test]
    fn check_states_use_the_protocol_spelling() {
        let all = [
            (CheckState::Ok, "ok"),
            (CheckState::Warn, "warn"),
            (CheckState::Fail, "fail"),
            (CheckState::Unknown, "unknown"),
        ];
        for (state, expected) in all {
            assert_eq!(state.as_str(), expected);
            assert_eq!(state.to_string(), expected);
        }
        assert_eq!(CheckName::LndSync.as_str(), "lnd_sync");
        assert_eq!(CheckName::Payouts.as_str(), "payouts");
        assert_eq!(CheckName::Relays.as_str(), "relays");
        assert_eq!(CheckName::CashuMint.as_str(), "cashu_mint");
        assert_eq!(CheckName::Database.as_str(), "database");
    }

    #[test]
    fn payouts_ok_without_any_payouts() {
        assert_eq!(
            payouts_check(Some(&PayoutFacts::default()), NOW),
            CheckState::Ok
        );
    }

    #[test]
    fn payouts_unknown_without_facts() {
        assert_eq!(payouts_check(None, NOW), CheckState::Unknown);
    }

    #[test]
    fn payouts_warn_on_a_settled_order_waiting_over_30_minutes() {
        let facts = PayoutFacts {
            oldest_settled_unpaid_age_secs: Some(1_801),
            last_success_at: Some(NOW - 60),
            ..Default::default()
        };
        assert_eq!(payouts_check(Some(&facts), NOW), CheckState::Warn);
        let facts = PayoutFacts {
            oldest_settled_unpaid_age_secs: Some(1_800),
            ..facts
        };
        assert_eq!(payouts_check(Some(&facts), NOW), CheckState::Ok);
    }

    /// A buyer whose wallet is offline makes mostrod retry for a while; that
    /// alone is not the node being degraded.
    #[test]
    fn a_settled_order_waiting_twenty_minutes_is_not_a_warning() {
        let facts = PayoutFacts {
            oldest_settled_unpaid_age_secs: Some(1_200),
            last_success_at: Some(NOW - 60),
            ..Default::default()
        };
        assert_eq!(payouts_check(Some(&facts), NOW), CheckState::Ok);
    }

    #[test]
    fn payouts_fail_when_stuck_two_hours_without_any_success() {
        // The 2026-10-14 incident: buyers waiting for hours, nothing paid.
        let facts = PayoutFacts {
            oldest_settled_unpaid_age_secs: Some(7_201),
            last_success_at: Some(NOW - 7_201),
            ..Default::default()
        };
        assert_eq!(payouts_check(Some(&facts), NOW), CheckState::Fail);
        let never = PayoutFacts {
            last_success_at: None,
            ..facts
        };
        assert_eq!(payouts_check(Some(&never), NOW), CheckState::Fail);
        // One order is old, but payouts are going out: only a warning.
        let paying = PayoutFacts {
            last_success_at: Some(NOW - 60),
            ..facts
        };
        assert_eq!(payouts_check(Some(&paying), NOW), CheckState::Warn);
    }

    #[test]
    fn payouts_fail_when_most_recent_attempts_fail() {
        let facts = PayoutFacts {
            succeeded_1h: 1,
            failed_1h: 2,
            failed_orders_1h: 2,
            ..Default::default()
        };
        assert_eq!(payouts_check(Some(&facts), NOW), CheckState::Fail);
        // Exactly half is not "more than half".
        let half = PayoutFacts {
            succeeded_1h: 2,
            failed_1h: 2,
            failed_orders_1h: 2,
            ..Default::default()
        };
        assert_eq!(payouts_check(Some(&half), NOW), CheckState::Ok);
        // Too few attempts to judge a rate.
        let few = PayoutFacts {
            failed_1h: 2,
            failed_orders_1h: 2,
            ..Default::default()
        };
        assert_eq!(payouts_check(Some(&few), NOW), CheckState::Ok);
    }

    /// Staging, 2026-10-15: one buyer's LN address failed all three retries
    /// of one order, then a new invoice paid. Four failures and one success
    /// in the hour, all on one order, is that buyer's problem, not ours.
    #[test]
    fn payouts_rate_needs_failures_on_more_than_one_order() {
        let one_order = PayoutFacts {
            succeeded_1h: 1,
            failed_1h: 4,
            failed_orders_1h: 1,
            last_success_at: Some(NOW - 60),
            ..Default::default()
        };
        assert_eq!(payouts_check(Some(&one_order), NOW), CheckState::Ok);
        let two_orders = PayoutFacts {
            failed_orders_1h: 2,
            ..one_order
        };
        assert_eq!(payouts_check(Some(&two_orders), NOW), CheckState::Fail);
    }

    /// ToRyVand on #1071: three `insufficient_balance` failures on the only
    /// payout of the hour are the node unable to pay, not one bad invoice.
    #[test]
    fn payouts_rate_counts_one_order_the_node_cannot_fund() {
        let facts = PayoutFacts {
            failed_1h: 3,
            failed_orders_1h: 1,
            insufficient_balance_1h: 3,
            ..Default::default()
        };
        assert_eq!(payouts_check(Some(&facts), NOW), CheckState::Fail);
        // One of them is enough: the node ran short at least once.
        let once = PayoutFacts {
            insufficient_balance_1h: 1,
            ..facts
        };
        assert_eq!(payouts_check(Some(&once), NOW), CheckState::Fail);
        let bad_invoice = PayoutFacts {
            insufficient_balance_1h: 0,
            ..facts
        };
        assert_eq!(payouts_check(Some(&bad_invoice), NOW), CheckState::Ok);
    }

    /// ToRyVand on #1071: a stale order can be a buyer this node left
    /// unpaid (an admin settle that failed between settling the hold invoice
    /// and paying, #961), not only residue from a previous node. It keeps
    /// the check at `warn` until the operator clears it, but never fails it.
    #[test]
    fn payouts_warn_while_stale_orders_are_left() {
        let facts = PayoutFacts {
            stale_settled_unpaid: 1,
            last_success_at: None,
            ..Default::default()
        };
        assert_eq!(payouts_check(Some(&facts), NOW), CheckState::Warn);
        // A recent order stuck as well still fails the check.
        let stuck = PayoutFacts {
            oldest_settled_unpaid_age_secs: Some(7_201),
            ..facts
        };
        assert_eq!(payouts_check(Some(&stuck), NOW), CheckState::Fail);
    }

    #[test]
    fn relays_check_follows_the_connections() {
        let long_ago = Some(NOW - RELAYS_MISSING_WARN_AFTER_SECS);
        assert_eq!(relays_check(3, 3, None, NOW), CheckState::Ok);
        assert_eq!(relays_check(3, 1, long_ago, NOW), CheckState::Warn);
        assert_eq!(relays_check(3, 0, long_ago, NOW), CheckState::Fail);
        assert_eq!(relays_check(0, 0, None, NOW), CheckState::Fail);
    }

    #[test]
    fn a_relay_missing_less_than_five_minutes_is_ok_but_none_connected_fails_at_once() {
        let just_now = Some(NOW - RELAYS_MISSING_WARN_AFTER_SECS + 1);
        assert_eq!(relays_check(3, 2, just_now, NOW), CheckState::Ok);
        assert_eq!(relays_check(3, 0, just_now, NOW), CheckState::Fail);
        // Not timed at all: read as long, never as short.
        assert_eq!(relays_check(3, 2, None, NOW), CheckState::Warn);
    }

    #[test]
    fn only_every_relay_connected_ends_a_missing_spell() {
        assert_eq!(relays_missing_since(None, 3, 2, NOW), Some(NOW));
        assert_eq!(relays_missing_since(Some(NOW), 3, 0, NOW + 60), Some(NOW));
        assert_eq!(relays_missing_since(Some(NOW), 3, 2, NOW + 120), Some(NOW));
        assert_eq!(relays_missing_since(Some(NOW), 3, 3, NOW + 180), None);
    }

    #[test]
    fn database_check_follows_the_probe() {
        assert_eq!(database_check(true), CheckState::Ok);
        assert_eq!(database_check(false), CheckState::Fail);
    }

    fn all_ok() -> Vec<(CheckName, CheckState)> {
        vec![
            (CheckName::LndSync, CheckState::Ok),
            (CheckName::Payouts, CheckState::Ok),
            (CheckName::Relays, CheckState::Ok),
            (CheckName::Database, CheckState::Ok),
        ]
    }

    fn with(name: CheckName, state: CheckState) -> Vec<(CheckName, CheckState)> {
        all_ok()
            .into_iter()
            .map(|(n, s)| if n == name { (n, state) } else { (n, s) })
            .collect()
    }

    #[test]
    fn status_is_ok_only_when_everything_is() {
        assert_eq!(derive_status(&all_ok(), false), HealthStatus::Ok);
        assert_eq!(derive_status(&all_ok(), true), HealthStatus::Degraded);
    }

    #[test]
    fn status_down_rules() {
        use CheckName::*;
        use CheckState::*;
        for (name, state) in [
            (LndSync, Fail),
            (LndSync, Unknown),
            (Database, Fail),
            (Relays, Fail),
        ] {
            assert_eq!(
                derive_status(&with(name, state), false),
                HealthStatus::Down,
                "{name:?} {state:?}"
            );
        }
    }

    #[test]
    fn status_degraded_rules() {
        use CheckName::*;
        use CheckState::*;
        for (name, state) in [
            (LndSync, Warn),
            (Payouts, Fail),
            (Payouts, Unknown),
            (Payouts, Warn),
            (Relays, Warn),
        ] {
            assert_eq!(
                derive_status(&with(name, state), false),
                HealthStatus::Degraded,
                "{name:?} {state:?}"
            );
        }
    }

    fn cashu(mint: CheckState) -> Vec<(CheckName, CheckState)> {
        vec![
            (CheckName::CashuMint, mint),
            (CheckName::Relays, CheckState::Ok),
            (CheckName::Database, CheckState::Ok),
        ]
    }

    #[test]
    fn cashu_status_follows_the_mint() {
        assert_eq!(
            derive_status(&cashu(CheckState::Ok), false),
            HealthStatus::Ok
        );
        assert_eq!(
            derive_status(&cashu(CheckState::Fail), false),
            HealthStatus::Down
        );
        assert_eq!(
            derive_status(&cashu(CheckState::Warn), false),
            HealthStatus::Degraded
        );
        assert_eq!(
            derive_status(&cashu(CheckState::Unknown), false),
            HealthStatus::Degraded
        );
    }

    #[test]
    fn tracker_keeps_since_while_the_state_holds() {
        let mut tracker = CheckTracker::default();
        assert_eq!(tracker.update(CheckName::Relays, CheckState::Ok, NOW), NOW);
        assert_eq!(
            tracker.update(CheckName::Relays, CheckState::Ok, NOW + 60),
            NOW
        );
        assert_eq!(
            tracker.update(CheckName::Relays, CheckState::Warn, NOW + 120),
            NOW + 120
        );
        assert_eq!(
            tracker.update(CheckName::Database, CheckState::Ok, NOW + 120),
            NOW + 120
        );
    }
}
