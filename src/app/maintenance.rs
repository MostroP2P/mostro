//! Maintenance ("drain") mode.
//!
//! While enabled the daemon refuses to open new escrow — `NewOrder`, `TakeBuy`
//! and `TakeSell` are answered with `CantDo(MaintenanceMode)` — but every
//! action that closes escrow keeps working, so open trades can finish on the
//! current Lightning node before the operator switches to a different one.
//! `drain_counters` reports what is still bound to that node.
//! Spec: `docs/MAINTENANCE_MODE_LN_MIGRATION.md` §3.2 / §3.4.

use crate::app::bond::types::BondState;
use crate::app::daemon_state;
use mostro_core::error::MostroError::{self, MostroInternalErr};
use mostro_core::error::ServiceError;
use mostro_core::prelude::{Action, Status};
use sqlx::SqlitePool;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::Mutex;

pub const KEY_ENABLED: &str = "maintenance_mode";
pub const KEY_REASON: &str = "maintenance_reason";
pub const KEY_SINCE: &str = "maintenance_since";

/// Process-wide maintenance flag, backed by `daemon_state`.
///
/// Cheap to clone; every clone observes the same flag. The persisted value is
/// authoritative: `set` writes the DB first and flips the atomic second, so a
/// crash in between is resolved by the next `load`. Writers are serialised by
/// `write_lock` for the whole DB-then-atomic sequence, so two concurrent
/// `set` calls cannot leave the row reflecting one and the flag the other.
#[derive(Clone, Debug, Default)]
pub struct MaintenanceState {
    enabled: Arc<AtomicBool>,
    write_lock: Arc<Mutex<()>>,
}

impl MaintenanceState {
    /// A disabled flag not backed by any row yet (tests, and the default for
    /// contexts that never load one).
    pub fn new() -> Self {
        Self::default()
    }

    /// Build the flag from the persisted `daemon_state` row.
    pub async fn load(pool: &SqlitePool) -> Result<Self, MostroError> {
        let enabled = daemon_state::get(pool, KEY_ENABLED)
            .await?
            .map(|v| v == "1")
            .unwrap_or(false);
        Ok(Self {
            enabled: Arc::new(AtomicBool::new(enabled)),
            write_lock: Arc::default(),
        })
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }

    /// Does the flag block this action? Only the three actions that open new
    /// escrow; everything on an existing order stays allowed (spec R4).
    pub fn blocks(&self, action: &Action) -> bool {
        self.is_enabled()
            && matches!(
                action,
                Action::NewOrder | Action::TakeBuy | Action::TakeSell
            )
    }

    /// Persist and apply a new value. `reason` is stored verbatim (or cleared)
    /// and `maintenance_since` is stamped on every enable.
    ///
    /// The three rows are written in one transaction and the in-memory flag
    /// flips only after it commits, all under `write_lock`. A failed commit
    /// leaves both the rows and the flag untouched.
    pub async fn set(
        &self,
        pool: &SqlitePool,
        enabled: bool,
        reason: Option<&str>,
    ) -> Result<(), MostroError> {
        let _guard = self.write_lock.lock().await;
        let since = if enabled {
            chrono::Utc::now().timestamp().to_string()
        } else {
            String::new()
        };
        let mut tx = pool
            .begin()
            .await
            .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
        daemon_state::set_in(&mut tx, KEY_ENABLED, if enabled { "1" } else { "0" }).await?;
        daemon_state::set_in(&mut tx, KEY_REASON, reason.unwrap_or_default()).await?;
        daemon_state::set_in(&mut tx, KEY_SINCE, &since).await?;
        tx.commit()
            .await
            .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
        self.enabled.store(enabled, Ordering::Release);
        Ok(())
    }
}

/// What is still bound to the connected Lightning node (spec §1.3).
/// `drained()` is true when the node can be switched.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DrainCounters {
    /// A — orders with a hold invoice in a non-terminal status.
    pub escrowed_orders: u32,
    /// B — buyer payouts in flight (`settled-hold-invoice` + payout hash).
    pub inflight_payouts: u32,
    /// C — successful orders whose dev fee is still unpaid.
    pub unpaid_dev_fees: u32,
    /// D — bond hold invoices still open (`requested` / `locked`).
    pub open_bonds: u32,
    /// E — bonds waiting for (or in the middle of) their payout.
    pub pending_bond_payouts: u32,
    /// Informational: pending orders hold no escrow and do not block a switch.
    pub pending_orders: u32,
}

impl DrainCounters {
    pub fn drained(&self) -> bool {
        self.escrowed_orders == 0
            && self.inflight_payouts == 0
            && self.unpaid_dev_fees == 0
            && self.open_bonds == 0
            && self.pending_bond_payouts == 0
    }
}

/// Order statuses that no longer need the node that issued their hold invoice.
fn terminal_statuses() -> [String; 7] {
    [
        Status::Canceled,
        Status::CanceledByAdmin,
        Status::SettledByAdmin,
        Status::CompletedByAdmin,
        Status::Expired,
        Status::Success,
        Status::CooperativelyCanceled,
    ]
    .map(|s| s.to_string())
}

async fn count(pool: &SqlitePool, sql: &'static str, binds: &[String]) -> Result<u32, MostroError> {
    let mut q = sqlx::query_scalar::<_, i64>(sql);
    for b in binds {
        q = q.bind(b.clone());
    }
    q.fetch_one(pool)
        .await
        .map(|n| n.max(0) as u32)
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))
}

/// Compute the counters with the exact predicates of spec §1.3.
pub async fn drain_counters(pool: &SqlitePool) -> Result<DrainCounters, MostroError> {
    let terminal = terminal_statuses();
    Ok(DrainCounters {
        // One placeholder per entry of `terminal_statuses()`.
        escrowed_orders: count(
            pool,
            "SELECT COUNT(*) FROM orders WHERE hash IS NOT NULL \
             AND status NOT IN (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            &terminal,
        )
        .await?,
        inflight_payouts: count(
            pool,
            "SELECT COUNT(*) FROM orders WHERE payout_payment_hash IS NOT NULL AND status = ?1",
            &[Status::SettledHoldInvoice.to_string()],
        )
        .await?,
        unpaid_dev_fees: count(
            pool,
            "SELECT COUNT(*) FROM orders WHERE dev_fee > 0 AND dev_fee_paid = 0 AND status = ?1",
            &[Status::Success.to_string()],
        )
        .await?,
        // D is exactly `BondState::is_active()`; keep the two in sync.
        open_bonds: count(
            pool,
            "SELECT COUNT(*) FROM bonds WHERE hash IS NOT NULL AND state IN (?1, ?2)",
            &[
                BondState::Requested.to_string(),
                BondState::Locked.to_string(),
            ],
        )
        .await?,
        pending_bond_payouts: count(
            pool,
            "SELECT COUNT(*) FROM bonds WHERE state = ?1",
            &[BondState::PendingPayout.to_string()],
        )
        .await?,
        pending_orders: count(
            pool,
            "SELECT COUNT(*) FROM orders WHERE status = ?1",
            &[Status::Pending.to_string()],
        )
        .await?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::bond::db::create_bond;
    use crate::app::bond::model::Bond;
    use crate::app::bond::types::BondRole;
    use uuid::Uuid;

    async fn pool() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        pool
    }

    async fn insert_order(
        pool: &SqlitePool,
        status: &str,
        hash: Option<&str>,
        payout_hash: Option<&str>,
        dev_fee: i64,
        dev_fee_paid: bool,
    ) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query(
            r#"INSERT INTO orders (id, kind, event_id, status, premium, payment_method,
                                   amount, fiat_code, fiat_amount, created_at, expires_at,
                                   failed_payment, payment_attempts, hash, payout_payment_hash,
                                   dev_fee, dev_fee_paid)
               VALUES (?1, 'sell', 'ev', ?2, 0, 'lightning', 100000, 'USD', 100,
                       1700000000, 1700086400, 0, 0, ?3, ?4, ?5, ?6)"#,
        )
        .bind(id)
        .bind(status)
        .bind(hash)
        .bind(payout_hash)
        .bind(dev_fee)
        .bind(dev_fee_paid)
        .execute(pool)
        .await
        .unwrap();
        id
    }

    /// Bonds carry a FK to `orders`, so each one gets its own parent order
    /// in a status that contributes nothing to the counters.
    async fn insert_bond(
        pool: &SqlitePool,
        state: BondState,
        hash: Option<&str>,
        payout: Option<&str>,
    ) {
        let order_id = insert_order(pool, "canceled", None, None, 0, false).await;
        let mut b = Bond::new_requested(order_id, "ab".repeat(32), BondRole::Maker, 1000);
        b.state = state.to_string();
        b.hash = hash.map(str::to_string);
        b.payout_payment_hash = payout.map(str::to_string);
        create_bond(pool, b).await.unwrap();
    }

    #[tokio::test]
    async fn maintenance_state_defaults_to_disabled_without_a_row() {
        let pool = pool().await;
        let state = MaintenanceState::load(&pool).await.unwrap();
        assert!(!state.is_enabled());
    }

    #[tokio::test]
    async fn maintenance_state_round_trips_through_daemon_state() {
        let pool = pool().await;
        let state = MaintenanceState::load(&pool).await.unwrap();
        state.set(&pool, true, Some("ln migration")).await.unwrap();
        assert!(state.is_enabled(), "atomic flips with the write");

        let fresh = MaintenanceState::load(&pool).await.unwrap();
        assert!(fresh.is_enabled(), "a fresh load sees the persisted value");
        assert_eq!(
            daemon_state::get(&pool, KEY_REASON)
                .await
                .unwrap()
                .as_deref(),
            Some("ln migration")
        );
        assert!(!daemon_state::get(&pool, KEY_SINCE)
            .await
            .unwrap()
            .unwrap()
            .is_empty());

        state.set(&pool, false, None).await.unwrap();
        assert!(!MaintenanceState::load(&pool).await.unwrap().is_enabled());
        assert_eq!(
            daemon_state::get(&pool, KEY_SINCE)
                .await
                .unwrap()
                .as_deref(),
            Some("")
        );
    }

    #[tokio::test]
    async fn set_leaves_flag_and_rows_untouched_when_the_write_fails() {
        let pool = pool().await;
        let state = MaintenanceState::load(&pool).await.unwrap();
        pool.close().await;
        assert!(state.set(&pool, true, Some("x")).await.is_err());
        assert!(!state.is_enabled(), "a failed write must not flip the flag");
    }

    /// Concurrent writers: whatever order they land in, the persisted row and
    /// the in-memory flag must agree at the end.
    #[tokio::test]
    async fn concurrent_sets_keep_row_and_flag_consistent() {
        let pool = pool().await;
        let state = MaintenanceState::load(&pool).await.unwrap();
        let mut tasks = tokio::task::JoinSet::new();
        for i in 0..32u32 {
            let (state, pool) = (state.clone(), pool.clone());
            tasks.spawn(async move { state.set(&pool, i % 2 == 0, None).await.unwrap() });
        }
        while let Some(r) = tasks.join_next().await {
            r.unwrap();
        }
        let persisted = daemon_state::get(&pool, KEY_ENABLED)
            .await
            .unwrap()
            .unwrap()
            == "1";
        assert_eq!(state.is_enabled(), persisted);
        let since = daemon_state::get(&pool, KEY_SINCE).await.unwrap().unwrap();
        assert_eq!(
            since.is_empty(),
            !persisted,
            "since must match the final mode"
        );
    }

    #[tokio::test]
    async fn clones_share_one_flag() {
        let pool = pool().await;
        let a = MaintenanceState::load(&pool).await.unwrap();
        let b = a.clone();
        a.set(&pool, true, None).await.unwrap();
        assert!(b.is_enabled());
    }

    #[test]
    fn blocks_only_the_escrow_opening_actions() {
        let state = MaintenanceState::new();
        assert!(!state.blocks(&Action::NewOrder), "disabled blocks nothing");
        state.enabled.store(true, Ordering::Release);
        for a in [Action::NewOrder, Action::TakeBuy, Action::TakeSell] {
            assert!(state.blocks(&a), "{a:?} must be blocked");
        }
        for a in [
            Action::Release,
            Action::Cancel,
            Action::FiatSent,
            Action::AddInvoice,
            Action::AddBondInvoice,
            Action::PayBondInvoice,
            Action::Dispute,
            Action::RateUser,
            Action::AdminCancel,
            Action::AdminSettle,
            Action::AdminTakeDispute,
            Action::AdminAddSolver,
            Action::Orders,
            Action::RestoreSession,
            Action::TradePubkey,
            Action::LastTradeIndex,
            Action::AddCashuEscrow,
        ] {
            assert!(!state.blocks(&a), "{a:?} must stay allowed");
        }
    }

    /// Predicate D must stay the SQL twin of `BondState::is_active()`.
    #[test]
    fn open_bond_predicate_matches_bond_state_is_active() {
        let counted = [BondState::Requested, BondState::Locked];
        for state in [
            BondState::Requested,
            BondState::Locked,
            BondState::Released,
            BondState::PendingPayout,
            BondState::Slashed,
            BondState::Forfeited,
            BondState::Failed,
        ] {
            assert_eq!(counted.contains(&state), state.is_active(), "{state}");
        }
    }

    #[tokio::test]
    async fn drain_counters_are_zero_on_an_empty_database() {
        let pool = pool().await;
        let c = drain_counters(&pool).await.unwrap();
        assert_eq!(c, DrainCounters::default());
        assert!(c.drained());
    }

    #[tokio::test]
    async fn drain_counters_reflects_each_predicate() {
        let pool = pool().await;
        let h = "aa".repeat(32);

        // A: escrowed, non-terminal.
        insert_order(&pool, "active", Some(&h), None, 0, false).await;
        let c = drain_counters(&pool).await.unwrap();
        assert_eq!((c.escrowed_orders, c.inflight_payouts), (1, 0));
        assert!(!c.drained());

        // B: in-flight buyer payout (also counted under A, non-terminal).
        insert_order(&pool, "settled-hold-invoice", Some(&h), Some(&h), 0, false).await;
        let c = drain_counters(&pool).await.unwrap();
        assert_eq!((c.escrowed_orders, c.inflight_payouts), (2, 1));

        // C: unpaid dev fee on a successful order.
        insert_order(&pool, "success", Some(&h), None, 50, false).await;
        let c = drain_counters(&pool).await.unwrap();
        assert_eq!((c.escrowed_orders, c.unpaid_dev_fees), (2, 1));

        // D / E: bonds.
        insert_bond(&pool, BondState::Locked, Some(&h), None).await;
        insert_bond(&pool, BondState::PendingPayout, Some(&h), Some(&h)).await;
        let c = drain_counters(&pool).await.unwrap();
        assert_eq!((c.open_bonds, c.pending_bond_payouts), (1, 1));

        // Informational only.
        insert_order(&pool, "pending", None, None, 0, false).await;
        let c = drain_counters(&pool).await.unwrap();
        assert_eq!(c.pending_orders, 1);
        assert_eq!(c.escrowed_orders, 2, "pending without hash is not escrow");
    }

    #[tokio::test]
    async fn drain_counters_ignore_terminal_residue() {
        let pool = pool().await;
        let h = "bb".repeat(32);
        // Terminal order that kept both hashes (post-finalisation clear lost).
        insert_order(&pool, "canceled-by-admin", Some(&h), Some(&h), 0, false).await;
        insert_order(&pool, "success", Some(&h), Some(&h), 50, true).await;
        // Slashed bond keeps payout_payment_hash as its idempotency record.
        insert_bond(&pool, BondState::Slashed, Some(&h), Some(&h)).await;
        insert_bond(&pool, BondState::Released, Some(&h), None).await;

        let c = drain_counters(&pool).await.unwrap();
        assert_eq!(c, DrainCounters::default());
        assert!(c.drained());
    }
}
