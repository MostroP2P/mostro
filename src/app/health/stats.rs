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

/// Bound on a background write of the last successful payout time.
const PERSIST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

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
    /// Record `value` at `at`. Usually the newest entry, but an outcome
    /// recovered by reconciliation carries LND's (older) time: the window
    /// stays ordered, so [`Window::recent`] drops it once it is out.
    fn push(&mut self, at: i64, value: T) {
        let index = self.entries.partition_point(|(t, _)| *t <= at);
        self.entries.insert(index, (at, value));
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
    /// The latest successful payout this process has seen, and whether it
    /// has been persisted. Kept in memory so a failed write never loses it:
    /// the health event uses it at once, and [`HealthStats::persist_last_success`]
    /// retries the write until it lands.
    last_success: Arc<Mutex<LastSuccess>>,
}

#[derive(Clone, Copy, Debug, Default)]
struct LastSuccess {
    at: Option<i64>,
    persisted: bool,
}

impl HealthStats {
    pub fn new() -> Self {
        Self::default()
    }

    /// A buyer payout reached `Succeeded` in LND. Memory only: it never
    /// touches the database, so it can sit on the payout finalization path
    /// without delaying or failing it. The time also becomes the latest
    /// successful payout (newest wins), to be persisted by
    /// [`HealthStats::persist_last_success`] (in the background, see
    /// [`HealthStats::persist_last_success_in_background`], and on every
    /// health evaluation).
    pub fn record_payout_success(&self, now: i64) {
        lock(&self.payouts).push(now, PayoutOutcome::Succeeded);
        let mut last = lock(&self.last_success);
        if last.at.is_none_or(|at| now >= at) {
            *last = LastSuccess {
                at: Some(now),
                persisted: false,
            };
        }
    }

    /// Persist the latest successful payout time off the caller's path, in
    /// a spawned task bounded to [`PERSIST_TIMEOUT`]. A failure only leaves it
    /// pending for the next attempt.
    pub fn persist_last_success_in_background(&self, pool: std::sync::Arc<SqlitePool>) {
        let stats = self.clone();
        tokio::spawn(async move {
            if tokio::time::timeout(PERSIST_TIMEOUT, stats.persist_last_success(&pool))
                .await
                .is_err()
            {
                warn!("Saving the last successful payout time did not finish in time");
            }
        });
    }

    /// Write the latest successful payout time if it is not persisted yet.
    /// Called after every success and on every health evaluation, so a time
    /// whose write failed reaches the database once it is available again.
    pub async fn persist_last_success(&self, pool: &SqlitePool) {
        let LastSuccess { at, persisted } = *lock(&self.last_success);
        let Some(at) = at.filter(|_| !persisted) else {
            return;
        };
        match save_last_success(pool, at).await {
            Ok(()) => {
                let mut last = lock(&self.last_success);
                // A newer success recorded meanwhile stays pending.
                if last.at == Some(at) {
                    last.persisted = true;
                }
            }
            Err(e) => warn!("Could not persist the last successful payout time: {e}"),
        }
    }

    /// The latest successful payout this process has seen, persisted or not.
    pub fn last_success_in_memory(&self) -> Option<i64> {
        lock(&self.last_success).at
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

/// Store `at` as the last successful payout, unless a later time is already
/// stored: concurrent writers (the payout watcher, a reconciliation, the
/// health job's retry) may finish in any order, and the stored value must
/// never move back.
async fn save_last_success(pool: &SqlitePool, at: i64) -> Result<(), MostroError> {
    sqlx::query(
        "INSERT INTO daemon_state (key, value, updated_at) VALUES (?1, ?2, ?3) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at \
         WHERE CAST(excluded.value AS INTEGER) > CAST(daemon_state.value AS INTEGER) \
            OR daemon_state.value = ''",
    )
    .bind(KEY_LAST_PAYOUT_SUCCESS_AT)
    .bind(at.to_string())
    .bind(chrono::Utc::now().timestamp())
    .execute(pool)
    .await
    .map(|_| ())
    .map_err(|e| {
        mostro_core::error::MostroError::MostroInternalErr(
            mostro_core::error::ServiceError::DbAccessError(e.to_string()),
        )
    })
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
        let stats = HealthStats::new();
        stats.record_payout_failure(T0, "no_route");
        stats.record_payout_success(T0 + 10);
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

    /// Codex review on #1066: an outcome recorded late at its own (older)
    /// time is in the window only while that time is.
    #[test]
    fn an_outcome_recorded_late_counts_at_its_own_time() {
        let stats = HealthStats::new();
        let now = T0 + 3 * WINDOW_SECS;
        stats.record_payout_success(now - 10);
        stats.record_payout_failure(now - 2 * WINDOW_SECS, "no_route");
        stats.record_payout_success(now - 3 * WINDOW_SECS);
        stats.record_payout_failure(now - 60, "no_route");

        let window = stats.payout_window(now);
        assert_eq!(window.succeeded, 1, "the hours-old success is out");
        assert_eq!(window.failed, 1, "the hours-old failure is out");
        assert_eq!(stats.last_success_in_memory(), Some(now - 10));
    }

    #[tokio::test]
    async fn last_payout_success_survives_a_restart() {
        let pool = pool().await;
        assert_eq!(last_payout_success_at(&pool).await.unwrap(), None);
        let first = HealthStats::new();
        first.record_payout_success(T0);
        first.persist_last_success(&pool).await;
        // A new process: fresh counters, same database.
        let restarted = HealthStats::new();
        assert_eq!(restarted.payout_window(T0 + 1).attempts(), 0);
        assert_eq!(last_payout_success_at(&pool).await.unwrap(), Some(T0));
    }

    /// Codex review on #1066: a success whose write fails is not lost.
    #[tokio::test]
    async fn a_success_whose_write_fails_is_kept_and_persisted_later() {
        let pool = pool().await;
        let block = "CREATE TRIGGER no_last_success BEFORE INSERT ON daemon_state \
             WHEN NEW.key = 'health_last_payout_success_at' \
             BEGIN SELECT RAISE(ABORT, 'injected'); END";
        sqlx::query(block).execute(&pool).await.unwrap();
        let stats = HealthStats::new();

        stats.record_payout_success(T0);
        stats.persist_last_success(&pool).await;

        assert_eq!(last_payout_success_at(&pool).await.unwrap(), None);
        assert_eq!(stats.last_success_in_memory(), Some(T0), "kept in memory");

        sqlx::query("DROP TRIGGER no_last_success")
            .execute(&pool)
            .await
            .unwrap();
        stats.persist_last_success(&pool).await;
        assert_eq!(last_payout_success_at(&pool).await.unwrap(), Some(T0));
    }

    /// Codex review on #1066: writes finishing out of order never move the
    /// stored time back.
    #[tokio::test]
    async fn the_stored_last_success_never_moves_back() {
        let pool = pool().await;
        save_last_success(&pool, T0 + 100).await.unwrap();
        // A slower writer with an older value lands afterwards.
        save_last_success(&pool, T0).await.unwrap();
        assert_eq!(last_payout_success_at(&pool).await.unwrap(), Some(T0 + 100));
        save_last_success(&pool, T0 + 200).await.unwrap();
        assert_eq!(last_payout_success_at(&pool).await.unwrap(), Some(T0 + 200));
    }

    #[tokio::test]
    async fn an_older_success_never_replaces_a_newer_one() {
        let pool = pool().await;
        let stats = HealthStats::new();
        stats.record_payout_success(T0 + 100);
        stats.record_payout_success(T0);
        stats.persist_last_success(&pool).await;
        assert_eq!(stats.last_success_in_memory(), Some(T0 + 100));
        assert_eq!(last_payout_success_at(&pool).await.unwrap(), Some(T0 + 100));
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
