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
use tokio::sync::{Mutex, Notify};

pub const KEY_ENABLED: &str = "maintenance_mode";
pub const KEY_REASON: &str = "maintenance_reason";
pub const KEY_SINCE: &str = "maintenance_since";
/// Written by the boot node-identity guard (Phase 4); read by the status RPC.
pub const KEY_LN_NODE_PUBKEY: &str = "ln_node_pubkey";

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
    /// Signalled after every successful `set`, so the info-event job can
    /// republish the `maintenance_mode` tag at once instead of waiting a
    /// full `publish_mostro_info_interval`. One consumer, so `notify_one`
    /// (which stores a permit) is used rather than `notify_waiters`.
    changed: Arc<Notify>,
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
            changed: Arc::default(),
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
        self.changed.notify_one();
        Ok(())
    }

    /// Resolves after the next successful `set` (or immediately if one
    /// happened since the last call). Used by the info-event job.
    pub async fn changed(&self) {
        self.changed.notified().await
    }
}

/// What is still bound to the connected Lightning node (spec §1.3).
/// `drained()` is true when the node can be switched.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DrainCounters {
    /// A — orders with a hold invoice in a non-terminal status, except a
    /// `settled-hold-invoice` whose payout has durably failed
    /// (`failed_payment = true`, no payout hash): its sats are already in
    /// Mostro's wallet and the retry / buyer's replacement invoice can be
    /// paid from any node. A freshly settled order still counts until
    /// `do_payment` records its claim (B) or a failure — stopping the
    /// daemon inside that window would strand the payout, since neither
    /// `find_inflight_payouts` nor `find_failed_payment` would see it.
    pub escrowed_orders: u32,
    /// B — buyer payouts in flight (`settled-hold-invoice` + payout hash).
    pub inflight_payouts: u32,
    /// C — dev-fee payouts the daemon has claimed or sent and not yet
    /// finalised (`dev_fee_payment_hash` marker set, `dev_fee_paid = 0`).
    /// A merely *unpaid* dev fee is not node-bound: it is an outgoing
    /// payment to a Lightning address the new node can make just as well.
    pub inflight_dev_fees: u32,
    /// D — bond hold invoices still open (`requested` / `locked`).
    pub open_bonds: u32,
    /// E — slashed-bond payouts in flight (`pending-payout` with a
    /// `payout_payment_hash`): `run_bond_payout_cycle` reconciles that hash
    /// with `track_payment_v2` on the node that sent it. A `pending-payout`
    /// bond still waiting for the winner's invoice is not node-bound — its
    /// HTLC was settled at slash time and the payout can go out from any
    /// node — so it does not block the switch.
    pub pending_bond_payouts: u32,
    /// Informational: pending orders hold no escrow and do not block a switch.
    pub pending_orders: u32,
}

impl DrainCounters {
    pub fn drained(&self) -> bool {
        self.escrowed_orders == 0
            && self.inflight_payouts == 0
            && self.inflight_dev_fees == 0
            && self.open_bonds == 0
            && self.pending_bond_payouts == 0
    }
}

/// Outcome of the boot node-identity guard (spec §3.6).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NodeIdentityDecision {
    /// No pubkey stored yet: recorded, continue.
    FirstBoot,
    /// Same node as last run: continue.
    Same,
    /// Different node but nothing bound to the old one: recorded, continue.
    ChangedDrained { previous: String },
    /// Different node with escrow still open on the old one and no
    /// override: the caller must refuse to start.
    ChangedWithOpenEscrow {
        previous: String,
        counters: DrainCounters,
    },
    /// Different node with open escrow, but `allow_node_change = true`:
    /// recorded, continue, affected rows knowingly left unresolved.
    ChangedOverridden {
        previous: String,
        counters: DrainCounters,
    },
}

impl NodeIdentityDecision {
    /// Whether the daemon may proceed with boot.
    pub fn allows_boot(&self) -> bool {
        !matches!(self, Self::ChangedWithOpenEscrow { .. })
    }
}

/// Pure decision rule of the guard, separated from IO so it can be tested
/// without LND. `stored` is the pubkey from the last run (if any), `current`
/// the connected node's, `counters` what is still bound to the old node.
pub fn check_node_identity(
    stored: Option<&str>,
    current: &str,
    counters: &DrainCounters,
    allow_override: bool,
) -> NodeIdentityDecision {
    match stored {
        None => NodeIdentityDecision::FirstBoot,
        Some(prev) if prev == current => NodeIdentityDecision::Same,
        Some(prev) if counters.drained() => NodeIdentityDecision::ChangedDrained {
            previous: prev.to_owned(),
        },
        Some(prev) if allow_override => NodeIdentityDecision::ChangedOverridden {
            previous: prev.to_owned(),
            counters: counters.clone(),
        },
        Some(prev) => NodeIdentityDecision::ChangedWithOpenEscrow {
            previous: prev.to_owned(),
            counters: counters.clone(),
        },
    }
}

/// Run the guard against the database: compare the connected node's pubkey
/// with the one persisted under `ln_node_pubkey`, compute the drain
/// counters only when they differ, and persist the new pubkey in every
/// case that allows boot. Never persists on refusal, so a later boot
/// against the old node still sees its own pubkey.
pub async fn node_identity_guard(
    pool: &SqlitePool,
    current: &str,
    allow_override: bool,
) -> Result<NodeIdentityDecision, MostroError> {
    let stored = daemon_state::get(pool, KEY_LN_NODE_PUBKEY).await?;
    let counters = match stored.as_deref() {
        Some(prev) if prev != current => drain_counters(pool).await?,
        _ => DrainCounters::default(),
    };
    let decision = check_node_identity(stored.as_deref(), current, &counters, allow_override);
    if decision.allows_boot() && !matches!(decision, NodeIdentityDecision::Same) {
        daemon_state::set(pool, KEY_LN_NODE_PUBKEY, current).await?;
    }
    Ok(decision)
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
        // One placeholder per entry of `terminal_statuses()`. A settled
        // order whose payout durably failed (retry state persisted, no
        // claim in flight) is not node-bound: the HTLC is already claimed
        // and the payout can be sent from any node. The unsettled → settled
        // → claimed/failed window stays counted (see `DrainCounters`).
        escrowed_orders: count(
            pool,
            "SELECT COUNT(*) FROM orders WHERE hash IS NOT NULL \
             AND status NOT IN (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
             AND NOT (status = ?8 AND failed_payment = 1 AND payout_payment_hash IS NULL)",
            &[
                terminal[0].clone(),
                terminal[1].clone(),
                terminal[2].clone(),
                terminal[3].clone(),
                terminal[4].clone(),
                terminal[5].clone(),
                terminal[6].clone(),
                Status::SettledHoldInvoice.to_string(),
            ],
        )
        .await?,
        inflight_payouts: count(
            pool,
            "SELECT COUNT(*) FROM orders WHERE payout_payment_hash IS NOT NULL AND status = ?1",
            &[Status::SettledHoldInvoice.to_string()],
        )
        .await?,
        // `dev_fee_payment_hash` is the claim marker / payment hash the
        // dev-fee job sets while it works an order and clears on failure
        // (`src/app/dev_fee.rs`); only that window is bound to the node.
        inflight_dev_fees: count(
            pool,
            "SELECT COUNT(*) FROM orders WHERE dev_fee_paid = 0 AND dev_fee_payment_hash IS NOT NULL",
            &[],
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
        // E is the bond twin of B: only a payout already dispatched is
        // tracked on the old node.
        pending_bond_payouts: count(
            pool,
            "SELECT COUNT(*) FROM bonds WHERE state = ?1 AND payout_payment_hash IS NOT NULL",
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
        insert_order_with_dev_fee_hash(pool, status, hash, payout_hash, dev_fee, dev_fee_paid, None)
            .await
    }

    async fn insert_order_with_dev_fee_hash(
        pool: &SqlitePool,
        status: &str,
        hash: Option<&str>,
        payout_hash: Option<&str>,
        dev_fee: i64,
        dev_fee_paid: bool,
        dev_fee_payment_hash: Option<&str>,
    ) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query(
            r#"INSERT INTO orders (id, kind, event_id, status, premium, payment_method,
                                   amount, fiat_code, fiat_amount, created_at, expires_at,
                                   failed_payment, payment_attempts, hash, payout_payment_hash,
                                   dev_fee, dev_fee_paid, dev_fee_payment_hash)
               VALUES (?1, 'sell', 'ev', ?2, 0, 'lightning', 100000, 'USD', 100,
                       1700000000, 1700086400, 0, 0, ?3, ?4, ?5, ?6, ?7)"#,
        )
        .bind(id)
        .bind(status)
        .bind(hash)
        .bind(payout_hash)
        .bind(dev_fee)
        .bind(dev_fee_paid)
        .bind(dev_fee_payment_hash)
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
    async fn set_wakes_a_changed_waiter_and_stores_a_permit() {
        let pool = pool().await;
        let state = MaintenanceState::load(&pool).await.unwrap();

        // Waiter registered before the change.
        let waiter = state.clone();
        let woke = tokio::spawn(async move { waiter.changed().await });
        tokio::task::yield_now().await;
        state.set(&pool, true, None).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), woke)
            .await
            .expect("changed() must resolve after set")
            .unwrap();

        // Change before anyone waits: the permit is kept.
        state.set(&pool, false, None).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), state.changed())
            .await
            .expect("a change before the wait must still be observed");

        // A failed write does not signal.
        let closed = MaintenanceState::load(&pool).await.unwrap();
        pool.close().await;
        assert!(closed.set(&pool, true, None).await.is_err());
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), closed.changed())
                .await
                .is_err(),
            "no notification on a failed set"
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

    fn open() -> DrainCounters {
        DrainCounters {
            escrowed_orders: 2,
            ..DrainCounters::default()
        }
    }

    #[test]
    fn node_guard_rule_covers_every_branch() {
        let none = DrainCounters::default();
        assert_eq!(
            check_node_identity(None, "02aa", &none, false),
            NodeIdentityDecision::FirstBoot
        );
        assert_eq!(
            check_node_identity(Some("02aa"), "02aa", &open(), false),
            NodeIdentityDecision::Same,
            "same node never refuses, whatever the counters"
        );
        assert_eq!(
            check_node_identity(Some("02aa"), "02bb", &none, false),
            NodeIdentityDecision::ChangedDrained {
                previous: "02aa".into()
            }
        );
        let refused = check_node_identity(Some("02aa"), "02bb", &open(), false);
        assert_eq!(
            refused,
            NodeIdentityDecision::ChangedWithOpenEscrow {
                previous: "02aa".into(),
                counters: open()
            }
        );
        assert!(!refused.allows_boot());
        let overridden = check_node_identity(Some("02aa"), "02bb", &open(), true);
        assert_eq!(
            overridden,
            NodeIdentityDecision::ChangedOverridden {
                previous: "02aa".into(),
                counters: open()
            }
        );
        assert!(overridden.allows_boot());
    }

    #[tokio::test]
    async fn node_guard_stores_pubkey_on_first_boot_and_accepts_same() {
        let pool = pool().await;
        assert_eq!(
            node_identity_guard(&pool, "02aa", false).await.unwrap(),
            NodeIdentityDecision::FirstBoot
        );
        assert_eq!(
            daemon_state::get(&pool, KEY_LN_NODE_PUBKEY)
                .await
                .unwrap()
                .as_deref(),
            Some("02aa")
        );
        // Open escrow does not matter while the node is the same.
        insert_order(&pool, "active", Some(&"aa".repeat(32)), None, 0, false).await;
        assert_eq!(
            node_identity_guard(&pool, "02aa", false).await.unwrap(),
            NodeIdentityDecision::Same
        );
    }

    #[tokio::test]
    async fn node_guard_rejects_new_pubkey_with_open_escrow_and_keeps_old_one() {
        let pool = pool().await;
        node_identity_guard(&pool, "02aa", false).await.unwrap();
        insert_order(&pool, "active", Some(&"aa".repeat(32)), None, 0, false).await;

        let decision = node_identity_guard(&pool, "02bb", false).await.unwrap();
        assert!(matches!(
            &decision,
            NodeIdentityDecision::ChangedWithOpenEscrow { previous, counters }
                if previous == "02aa" && counters.escrowed_orders == 1
        ));
        assert!(!decision.allows_boot());
        assert_eq!(
            daemon_state::get(&pool, KEY_LN_NODE_PUBKEY)
                .await
                .unwrap()
                .as_deref(),
            Some("02aa"),
            "a refused boot must not overwrite the stored pubkey"
        );
    }

    #[tokio::test]
    async fn node_guard_allows_new_pubkey_when_drained() {
        let pool = pool().await;
        node_identity_guard(&pool, "02aa", false).await.unwrap();
        insert_order(&pool, "success", Some(&"aa".repeat(32)), None, 0, true).await;

        let decision = node_identity_guard(&pool, "02bb", false).await.unwrap();
        assert_eq!(
            decision,
            NodeIdentityDecision::ChangedDrained {
                previous: "02aa".into()
            }
        );
        assert_eq!(
            daemon_state::get(&pool, KEY_LN_NODE_PUBKEY)
                .await
                .unwrap()
                .as_deref(),
            Some("02bb")
        );
    }

    #[tokio::test]
    async fn node_guard_override_records_new_pubkey_despite_open_escrow() {
        let pool = pool().await;
        node_identity_guard(&pool, "02aa", false).await.unwrap();
        insert_order(&pool, "active", Some(&"aa".repeat(32)), None, 0, false).await;

        let decision = node_identity_guard(&pool, "02bb", true).await.unwrap();
        assert!(matches!(
            decision,
            NodeIdentityDecision::ChangedOverridden { .. }
        ));
        assert!(decision.allows_boot());
        assert_eq!(
            daemon_state::get(&pool, KEY_LN_NODE_PUBKEY)
                .await
                .unwrap()
                .as_deref(),
            Some("02bb")
        );
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
        let inflight =
            insert_order(&pool, "settled-hold-invoice", Some(&h), Some(&h), 0, false).await;
        let c = drain_counters(&pool).await.unwrap();
        assert_eq!((c.escrowed_orders, c.inflight_payouts), (2, 1));

        // Freshly settled, payout not yet claimed nor failed: still under A —
        // stopping the daemon here would strand the payout.
        let fresh = insert_order(&pool, "settled-hold-invoice", Some(&h), None, 0, false).await;
        let c = drain_counters(&pool).await.unwrap();
        assert_eq!((c.escrowed_orders, c.inflight_payouts), (3, 1));

        // A retry re-claimed the payout: `failed_payment` set but a hash in
        // flight. Still node-bound, still under A and B.
        sqlx::query("UPDATE orders SET failed_payment = 1, payment_attempts = 2 WHERE id = ?")
            .bind(inflight)
            .execute(&pool)
            .await
            .unwrap();
        let c = drain_counters(&pool).await.unwrap();
        assert_eq!((c.escrowed_orders, c.inflight_payouts), (3, 1));

        // Payout durably failed (retry state persisted, waiting for the
        // buyer's replacement invoice): binds nothing, any node can pay it.
        sqlx::query("UPDATE orders SET failed_payment = 1, payment_attempts = 3 WHERE id = ?")
            .bind(fresh)
            .execute(&pool)
            .await
            .unwrap();
        let c = drain_counters(&pool).await.unwrap();
        assert_eq!((c.escrowed_orders, c.inflight_payouts), (2, 1));

        // C: dev fee claimed / in flight (marker set). A merely unpaid dev
        // fee with no marker is NOT node-bound and must not count.
        insert_order(&pool, "success", Some(&h), None, 50, false).await;
        let c = drain_counters(&pool).await.unwrap();
        assert_eq!((c.escrowed_orders, c.inflight_dev_fees), (2, 0));
        insert_order_with_dev_fee_hash(
            &pool,
            "success",
            Some(&h),
            None,
            50,
            false,
            Some("PENDING-x"),
        )
        .await;
        insert_order_with_dev_fee_hash(&pool, "success", Some(&h), None, 50, false, Some(&h)).await;
        let c = drain_counters(&pool).await.unwrap();
        assert_eq!((c.escrowed_orders, c.inflight_dev_fees), (2, 2));

        // D / E: bonds.
        insert_bond(&pool, BondState::Locked, Some(&h), None).await;
        insert_bond(&pool, BondState::PendingPayout, Some(&h), Some(&h)).await;
        let c = drain_counters(&pool).await.unwrap();
        assert_eq!((c.open_bonds, c.pending_bond_payouts), (1, 1));

        // A slashed bond still waiting for the winner's invoice: HTLC already
        // settled, payout not dispatched — payable from any node, not E.
        insert_bond(&pool, BondState::PendingPayout, Some(&h), None).await;
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
        insert_order_with_dev_fee_hash(&pool, "success", Some(&h), Some(&h), 50, true, Some(&h))
            .await;
        // Slashed bond keeps payout_payment_hash as its idempotency record.
        insert_bond(&pool, BondState::Slashed, Some(&h), Some(&h)).await;
        insert_bond(&pool, BondState::Released, Some(&h), None).await;

        let c = drain_counters(&pool).await.unwrap();
        assert_eq!(c, DrainCounters::default());
        assert!(c.drained());
    }
}
