//! Rolling counters behind the health event's `payouts` and `dev_fee`
//! content (issue #1063).
//!
//! Only recent rates are needed, so the windows live in memory and start
//! empty after a restart. The one value that must survive a restart, when the
//! last buyer payout succeeded, is persisted in `daemon_state`.

use crate::app::daemon_state;
use fedimint_tonic_lnd::lnrpc::PaymentFailureReason;
use mostro_core::error::MostroError;
use sqlx::SqlitePool;
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, LazyLock, Mutex};
use tracing::warn;

/// Length of every `_1h` window, in seconds (protocol: `window_secs`).
pub const WINDOW_SECS: i64 = 3_600;

/// `daemon_state` key of the last successful buyer payout (unix seconds).
pub const KEY_LAST_PAYOUT_SUCCESS_AT: &str = "health_last_payout_success_at";

/// Failure reason for a failure that cannot be mapped to one of LND's.
pub const REASON_OTHER: &str = "other";

/// Upper bound on the entries a window keeps, whatever the rate. A node
/// failing faster than this per hour reports a capped (still alarming)
/// count instead of growing without bound.
const MAX_WINDOW_ENTRIES: usize = 100_000;

/// The protocol key for an LND `PaymentFailureReason` (lowercase, without
/// the `FAILURE_REASON_` prefix), or [`REASON_OTHER`].
pub fn failure_reason_key(reason: i32) -> &'static str {
    match PaymentFailureReason::try_from(reason) {
        Ok(PaymentFailureReason::FailureReasonTimeout) => "timeout",
        Ok(PaymentFailureReason::FailureReasonNoRoute) => "no_route",
        Ok(PaymentFailureReason::FailureReasonError) => "error",
        Ok(PaymentFailureReason::FailureReasonIncorrectPaymentDetails) => {
            "incorrect_payment_details"
        }
        Ok(PaymentFailureReason::FailureReasonInsufficientBalance) => "insufficient_balance",
        Ok(PaymentFailureReason::FailureReasonCanceled) => "canceled",
        _ => REASON_OTHER,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum PayoutOutcome {
    Succeeded,
    Failed(&'static str),
}

/// Buyer payout attempts in the last hour.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PayoutWindow {
    pub succeeded: u64,
    pub failed: u64,
    /// Failed attempts by reason; reasons with no failures are absent.
    pub failure_reasons: BTreeMap<String, u64>,
}

impl PayoutWindow {
    pub fn attempts(&self) -> u64 {
        self.succeeded + self.failed
    }
}

/// Timestamps of events in a sliding window, oldest first.
#[derive(Debug)]
struct Window<T> {
    entries: VecDeque<(i64, T)>,
}

impl<T> Default for Window<T> {
    fn default() -> Self {
        Self {
            entries: VecDeque::new(),
        }
    }
}

impl<T> Window<T> {
    fn push(&mut self, at: i64, value: T) {
        self.entries.push_back((at, value));
        while self.entries.len() > MAX_WINDOW_ENTRIES {
            self.entries.pop_front();
        }
    }

    /// Drop what is older than [`WINDOW_SECS`] at `now` and return the rest.
    fn recent(&mut self, now: i64) -> impl Iterator<Item = &T> {
        while self
            .entries
            .front()
            .is_some_and(|(at, _)| now.saturating_sub(*at) > WINDOW_SECS)
        {
            self.entries.pop_front();
        }
        self.entries.iter().map(|(_, value)| value)
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Buyer payout outcomes, shared through `AppContext::health_stats()`.
/// Cheap to clone; every clone sees the same counters.
#[derive(Clone, Debug, Default)]
pub struct HealthStats {
    payouts: Arc<Mutex<Window<PayoutOutcome>>>,
}

impl HealthStats {
    pub fn new() -> Self {
        Self::default()
    }

    /// A buyer payout reached `Succeeded` in LND. Also persists the time as
    /// the last successful payout; a failed write is logged, not propagated,
    /// because the payout itself is not affected.
    pub async fn record_payout_success(&self, pool: &SqlitePool, now: i64) {
        lock(&self.payouts).push(now, PayoutOutcome::Succeeded);
        if let Err(e) = daemon_state::set(pool, KEY_LAST_PAYOUT_SUCCESS_AT, &now.to_string()).await
        {
            warn!("Could not persist the last successful payout time: {e}");
        }
    }

    /// A buyer payout reached `Failed` in LND, with this protocol reason key
    /// (see [`failure_reason_key`]).
    pub fn record_payout_failure(&self, now: i64, reason: &'static str) {
        lock(&self.payouts).push(now, PayoutOutcome::Failed(reason));
    }

    /// Outcomes in the last [`WINDOW_SECS`] at `now`.
    pub fn payout_window(&self, now: i64) -> PayoutWindow {
        let mut guard = lock(&self.payouts);
        let mut window = PayoutWindow::default();
        for outcome in guard.recent(now) {
            match outcome {
                PayoutOutcome::Succeeded => window.succeeded += 1,
                PayoutOutcome::Failed(reason) => {
                    window.failed += 1;
                    *window
                        .failure_reasons
                        .entry((*reason).to_string())
                        .or_default() += 1;
                }
            }
        }
        window
    }
}

/// When the last buyer payout succeeded, from `daemon_state`. `None` if
/// there was never one (or it predates this feature).
pub async fn last_payout_success_at(pool: &SqlitePool) -> Result<Option<i64>, MostroError> {
    Ok(daemon_state::get(pool, KEY_LAST_PAYOUT_SUCCESS_AT)
        .await?
        .and_then(|value| value.parse::<i64>().ok()))
}

/// Failed dev fee attempts. Process-wide because the dev fee job works on the
/// database pool alone, without an `AppContext`; it is only ever read by the
/// health event.
static DEV_FEE_FAILURES: LazyLock<Mutex<Window<()>>> = LazyLock::new(Mutex::default);

/// A dev fee attempt failed (resolution or payment).
pub fn record_dev_fee_failure(now: i64) {
    lock(&DEV_FEE_FAILURES).push(now, ());
}

/// Failed dev fee attempts in the last [`WINDOW_SECS`] at `now`.
pub fn dev_fee_failures_1h(now: i64) -> u64 {
    lock(&DEV_FEE_FAILURES).recent(now).count() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: i64 = 1_792_000_000;

    async fn pool() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        pool
    }

    #[test]
    fn failure_reasons_use_the_protocol_keys() {
        assert_eq!(failure_reason_key(1), "timeout");
        assert_eq!(failure_reason_key(2), "no_route");
        assert_eq!(failure_reason_key(3), "error");
        assert_eq!(failure_reason_key(4), "incorrect_payment_details");
        assert_eq!(failure_reason_key(5), "insufficient_balance");
        assert_eq!(failure_reason_key(6), "canceled");
        // FAILURE_REASON_NONE on a failed payment, or a reason from a newer
        // LND, cannot be mapped.
        assert_eq!(failure_reason_key(0), REASON_OTHER);
        assert_eq!(failure_reason_key(99), REASON_OTHER);
    }

    #[tokio::test]
    async fn payout_window_counts_the_last_hour_only() {
        let pool = pool().await;
        let stats = HealthStats::new();
        stats.record_payout_failure(T0, "no_route");
        stats.record_payout_success(&pool, T0 + 10).await;
        stats.record_payout_failure(T0 + 20, "incorrect_payment_details");
        stats.record_payout_failure(T0 + 30, "incorrect_payment_details");

        let window = stats.payout_window(T0 + 60);
        assert_eq!(window.succeeded, 1);
        assert_eq!(window.failed, 3);
        assert_eq!(window.attempts(), 4);
        assert_eq!(
            window.failure_reasons.get("incorrect_payment_details"),
            Some(&2)
        );
        assert_eq!(window.failure_reasons.get("no_route"), Some(&1));

        // An hour after the first failure it has left the window.
        let window = stats.payout_window(T0 + WINDOW_SECS + 1);
        assert_eq!(window.failed, 2);
        assert_eq!(window.failure_reasons.get("no_route"), None);
    }

    #[tokio::test]
    async fn last_payout_success_survives_a_restart() {
        let pool = pool().await;
        assert_eq!(last_payout_success_at(&pool).await.unwrap(), None);
        HealthStats::new().record_payout_success(&pool, T0).await;
        // A new process: fresh counters, same database.
        let restarted = HealthStats::new();
        assert_eq!(restarted.payout_window(T0 + 1).attempts(), 0);
        assert_eq!(last_payout_success_at(&pool).await.unwrap(), Some(T0));
    }

    #[test]
    fn clones_share_their_counters() {
        let stats = HealthStats::new();
        stats.clone().record_payout_failure(T0, "timeout");
        assert_eq!(stats.payout_window(T0).failed, 1);
    }

    #[test]
    fn dev_fee_failures_are_counted_per_hour() {
        // Process-wide: other tests may add failures concurrently, so only
        // assert on what this test adds, far from any real clock.
        let far = 4_000_000_000;
        let before = dev_fee_failures_1h(far);
        record_dev_fee_failure(far);
        record_dev_fee_failure(far);
        assert_eq!(dev_fee_failures_1h(far) - before, 2);
    }
}
