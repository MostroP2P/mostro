//! Phase 2 — solver-directed dispute slash.
//!
//! Translates a [`BondResolution`] payload carried by `Action::AdminSettle`
//! / `Action::AdminCancel` into concrete bond transitions:
//!
//! - **Slashed sides** have their hold invoice **immediately settled**
//!   (`settle_hold_invoice(preimage)`) and the row moves
//!   `Locked` → `PendingPayout`, with `slashed_reason`, `slashed_at`, and
//!   `node_share_sats` snapshotted in the same `UPDATE` so a later config
//!   change or daemon restart cannot rebalance the split. The HTLC is
//!   claimed by Mostro **here**, not asynchronously by Phase 3.
//! - **Non-slashed sides** are released exactly as Phase 1 did
//!   (`cancel_hold_invoice` + state = `Released`).
//!
//! The recipient payout (asking the winning counterparty for a bolt11,
//! `send_payment` retries, forfeiture on the long-stop window) is still
//! the job of Phase 3 (`job_process_bond_payouts`). Phase 3 no longer
//! settles HTLCs — by the time it picks up a `PendingPayout` row, the
//! sats are already in Mostro's wallet.
//!
//! ## Flow contract
//!
//! Handlers (`admin_settle_action`, `admin_cancel_action`) call
//! [`validate_bond_resolution`] **before** any trade-side mutation. If the
//! solver requested `slash_*=true` for a side with no `Locked` bond row,
//! the validator returns `MostroCantDo(InvalidPayload)`; the handler
//! propagates that and the trade resolution does not run — the solver
//! resends a corrected payload. After the trade-side mutation succeeds,
//! the handler calls [`apply_bond_resolution`] to perform the transitions.
//!
//! ## Feature-gate behaviour
//!
//! These functions are safe to call even when the anti-abuse bond feature
//! is disabled. `find_active_bonds_for_order` returns an empty set when
//! no bonds exist for the order, and both functions then no-op. This
//! preserves the Phase 1 invariant that a `null` payload + no bond rows
//! yields exactly the legacy behaviour.

use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::str::FromStr;

use chrono::Utc;
use mostro_core::error::{
    CantDoReason,
    MostroError::{self, MostroCantDo, MostroInternalErr},
    ServiceError,
};
use mostro_core::message::{Action, BondResolution, Message, Payload};
use mostro_core::order::{Order, SmallOrder, Status};
use nostr_sdk::prelude::PublicKey;
use sqlx::{Pool, Sqlite};
use tracing::{info, warn};
use uuid::Uuid;

use super::db::find_active_bonds_for_order;
use super::flow::{release_bond, release_bonds_for_order_or_warn};
use super::math::compute_node_share;
use super::model::Bond;
use super::types::{BondSlashReason, BondState};
use crate::config::settings::Settings;
use crate::config::types::AntiAbuseBondSettings;
use crate::lightning::LndConnector;
use crate::util::enqueue_order_msg;

/// Minimal LND-side capability the slash path needs: settle a hold
/// invoice by preimage. Mirrors the [`crate::app::cancel::CancelLightning`]
/// pattern so tests can pass a stub instead of a live `LndConnector`.
pub trait SettleLightning {
    fn settle_hold_invoice<'a>(
        &'a mut self,
        preimage: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), MostroError>> + Send + 'a>>;
}

impl SettleLightning for LndConnector {
    fn settle_hold_invoice<'a>(
        &'a mut self,
        preimage: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), MostroError>> + Send + 'a>> {
        Box::pin(async move {
            LndConnector::settle_hold_invoice(self, preimage)
                .await
                .map(|_| ())
        })
    }
}

/// Classify a `settle_hold_invoice` failure: an "already settled"
/// response is treated as success so admin retries (where the row is
/// still `Locked` but the HTLC is already claimed) drive the bond into
/// `PendingPayout` instead of looping forever on a benign error.
pub(super) fn is_already_settled_error(err: &MostroError) -> bool {
    let s = err.to_string().to_lowercase();
    s.contains("already settled")
        || s.contains("invoice already settled")
        || s.contains("code=alreadyexists")
}

/// Which trade-flow side a slash flag is targeting. Internal helper —
/// callers think in `BondResolution::slash_seller` / `slash_buyer`
/// terms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
    Seller,
    Buyer,
}

/// Extract a [`BondResolution`] from an admin message, defaulting to
/// "release all bonds" when the payload is absent, `null`, or carries
/// a different shape.
///
/// `MessageKind::verify` upstream already rejects `BondResolution` on
/// actions other than `AdminSettle` / `AdminCancel` (see `mostro-core`
/// 0.11.0 §verify), so by the time we're here the only "wrong shape"
/// case is the legitimate "no payload at all" — admin clients on the
/// pre-Phase-2 wire format. Defaulting to `false`/`false` is the
/// behaviour Phase 1 had.
pub fn extract_bond_resolution(msg: &Message) -> BondResolution {
    match &msg.get_inner_message_kind().payload {
        Some(Payload::BondResolution(br)) => br.clone(),
        _ => BondResolution {
            slash_seller: false,
            slash_buyer: false,
        },
    }
}

/// Pre-flight validation for a [`BondResolution`] payload.
///
/// Must be called **before** any trade-side mutation
/// (`settle_seller_hold_invoice`, `cancel_hold_invoice`,
/// `update_order_event`, …): if validation fails, the admin handler
/// returns `MostroCantDo(InvalidPayload)` and the trade itself is not
/// settled or cancelled. The solver is expected to resend a corrected
/// directive — see §7.3 step 3 of `docs/ANTI_ABUSE_BOND.md`.
///
/// Returns:
/// - `Ok(())` when no slash is requested (`null` payload ≡ both flags
///   `false`), or when every requested slash maps to a side with a
///   `Locked` bond row.
/// - `Err(MostroCantDo(InvalidPayload))` when any requested slash targets
///   a side that has no `Locked` bond. This naturally covers
///   "feature disabled / `apply_to=none` / no bond posted" without a
///   separate config check — the absence of the row is the signal.
pub async fn validate_bond_resolution(
    pool: &Pool<Sqlite>,
    order: &Order,
    resolution: &BondResolution,
) -> Result<(), MostroError> {
    if !resolution.slash_seller && !resolution.slash_buyer {
        return Ok(());
    }
    let bonds = find_active_bonds_for_order(pool, order.id).await?;
    if resolution.slash_seller && resolve_locked_bond(order, &bonds, Side::Seller).is_none() {
        return Err(MostroCantDo(CantDoReason::InvalidPayload));
    }
    if resolution.slash_buyer && resolve_locked_bond(order, &bonds, Side::Buyer).is_none() {
        return Err(MostroCantDo(CantDoReason::InvalidPayload));
    }
    Ok(())
}

/// Apply a validated [`BondResolution`] to every active bond on the order.
///
/// For each currently active bond:
/// - if the bond's owner is on the slashed side(s), **settle the hold
///   invoice immediately** (`settle_hold_invoice(preimage)`, claiming
///   the sats into Mostro's wallet) and then CAS
///   `state='locked' → state='pending-payout'` that also writes
///   `slashed_reason`, `slashed_at`, and `node_share_sats` in the same
///   statement. The split snapshot is intentionally frozen at this
///   moment — Phase 3's payout job never reads `slash_node_share_pct`
///   from config; it reads `node_share_sats` from the row.
/// - otherwise, release the bond exactly as Phase 1 did
///   (`cancel_hold_invoice` + state = `Released`).
///
/// **Ordering invariant**: settle MUST succeed before the CAS runs. If
/// settle fails (and is not the benign "already settled" idempotent
/// case), the row stays `Locked` and a future admin retry will re-run
/// the slash. Doing settle before CAS means a partial failure leaves a
/// retry-able state instead of a `PendingPayout` row whose HTLC is
/// still encumbered.
///
/// Idempotent: a bond that already moved out of `Locked` (e.g. a
/// duplicate admin call or a concurrent path) is left untouched by the
/// CAS, and the loop continues to the next bond. Settling an
/// already-settled HTLC is also benign (LND returns AlreadyExists,
/// which [`is_already_settled_error`] classifies as success).
///
/// `reason` is `LostDispute` in Phase 2 (called from admin handlers);
/// Phase 4 (timeout slash) will reuse this helper with `Timeout`.
pub async fn apply_bond_resolution<L: SettleLightning + Send>(
    pool: &Pool<Sqlite>,
    ln_client: &mut L,
    order: &Order,
    resolution: &BondResolution,
    reason: BondSlashReason,
) -> Result<(), MostroError> {
    let bonds = find_active_bonds_for_order(pool, order.id).await?;
    if bonds.is_empty() {
        return Ok(());
    }

    let mut slashed_ids: HashSet<Uuid> = HashSet::new();
    if resolution.slash_seller {
        if let Some(bond) = resolve_locked_bond(order, &bonds, Side::Seller) {
            slashed_ids.insert(bond.id);
        }
        // No-op if a Locked bond is missing: validation should have run
        // before any trade-side mutation. Reaching here with a missing
        // bond means a concurrent path (release, slash, expiry) raced
        // between validate and apply — letting the loop fall through to
        // the release branch on whatever remains is the safe outcome.
    }
    if resolution.slash_buyer {
        if let Some(bond) = resolve_locked_bond(order, &bonds, Side::Buyer) {
            slashed_ids.insert(bond.id);
        }
    }

    // Snapshot the split percentage *once* per call. Phase 3 will read
    // `node_share_sats` off each row; we never recompute it after the
    // transition.
    let node_share_pct = Settings::get_bond().map_or(0.0, |c| c.slash_node_share_pct);

    for bond in bonds.iter() {
        if slashed_ids.contains(&bond.id) {
            slash_one(pool, ln_client, bond, reason, node_share_pct).await;
        } else {
            // Non-slashed bonds on the same order: release with the
            // Phase 1 contract. `release_bond` is best-effort and
            // tolerant of transient LND failures.
            if let Err(e) = release_bond(pool, bond).await {
                warn!(
                    bond_id = %bond.id,
                    order_id = %order.id,
                    "apply_bond_resolution: release_bond failed: {}", e
                );
            }
        }
    }

    Ok(())
}

/// Phase 4 — timeout-slash dispatch for the scheduler's
/// `job_cancel_orders`.
///
/// Given the **pre-cancel** snapshot of an order whose waiting-state
/// deadline elapsed, decide — from the waiting state alone (the §9.2
/// responsibility table) and the node's bond policy — whether the
/// responsible party's bond is slashed with `BondSlashReason::Timeout`,
/// or every bond on the order is simply released (the Phase 1 "always
/// release" behaviour).
///
/// Responsibility maps directly from the waiting state:
/// `WaitingBuyerInvoice → buyer`, `WaitingPayment → seller`. The
/// buyer/seller → bond-row resolution then reuses the §3.1 order-kind
/// mapping baked into [`apply_bond_resolution`] (a slash flag is matched
/// against `order.buyer_pubkey` / `order.seller_pubkey`, which equal the
/// bonded taker's trade pubkey). So under `apply_to = "take"` only the
/// taker side ever carries a bond, and the maker-responsible rows of the
/// §9.2 table fall through to release.
///
/// A slash happens **only** when all of the following hold:
/// - the feature is enabled, `slash_on_waiting_timeout = true`, and
///   `apply_to` covers the taker;
/// - the order is in a waiting state;
/// - the responsible party holds a `Locked` bond.
///
/// Otherwise every active bond is released. This preserves today's
/// behaviour when the feature is off, when the bond belongs to the
/// non-responsible party, or when no bond exists — and it is the path
/// that drains stray bonds left over from a previously-enabled period,
/// so it is **not** gated on the current feature flag.
///
/// Returns `Ok(Some(bond))` with the slashed bond row when a timeout
/// slash actually landed (HTLC settled + row in `PendingPayout`), so the
/// caller can notify the slashed user; `Ok(None)` otherwise. The slash is
/// confirmed by re-reading the row: [`apply_bond_resolution`] is
/// best-effort and leaves the bond `Locked` on a transient settle
/// failure, so a `Some` return guarantees the bond was really forfeited
/// and the notice is truthful.
///
/// `order` MUST carry the pre-cancel waiting status and the trade
/// pubkeys; call this from the persist-success branch that replaces the
/// Phase 1 release call. `bond_cfg` is the node's `[anti_abuse_bond]`
/// config (the scheduler passes `Settings::get_bond()`); it is taken as a
/// parameter rather than read from the global so the gate is unit-testable
/// without mutating process-wide state.
pub async fn slash_or_release_on_timeout<L: SettleLightning + Send>(
    pool: &Pool<Sqlite>,
    ln_client: &mut L,
    order: &Order,
    bond_cfg: Option<&AntiAbuseBondSettings>,
) -> Result<Option<Bond>, MostroError> {
    // Responsible side from the waiting state. Anything else is not a
    // Phase 4 trigger — release defensively and bail. (The scheduler only
    // calls this for waiting-state orders, but we never assume it.)
    let side = match order.get_order_status() {
        Ok(Status::WaitingBuyerInvoice) => Side::Buyer,
        Ok(Status::WaitingPayment) => Side::Seller,
        _ => {
            release_bonds_for_order_or_warn(pool, order.id, "scheduler_timeout").await;
            return Ok(None);
        }
    };

    // Gate the slash. `apply_to` is a posting-timing switch; Phase 4 is
    // taker-only, so we check `applies_to_taker` (Phase 7 widens this to
    // the maker). When the gate is closed we still release — bonds left
    // over from a prior enabled period must drain regardless.
    let slash_armed = bond_cfg
        .is_some_and(|c| c.enabled && c.slash_on_waiting_timeout && c.apply_to.applies_to_taker());
    if !slash_armed {
        release_bonds_for_order_or_warn(pool, order.id, "scheduler_timeout").await;
        return Ok(None);
    }

    // Does the responsible party hold a `Locked` bond?
    let bonds = find_active_bonds_for_order(pool, order.id).await?;
    let Some(responsible) = resolve_locked_bond(order, &bonds, side).cloned() else {
        // Responsible party has no bond (e.g. the maker under
        // `apply_to = take`), or the bond already moved out of `Locked`.
        // No slash; release whatever is still active on the order.
        release_bonds_for_order_or_warn(pool, order.id, "scheduler_timeout").await;
        return Ok(None);
    };

    // Reuse the Phase 2 primitive. The `BondResolution` names the
    // responsible side; `apply_bond_resolution` settles that bond's HTLC
    // + CAS → PendingPayout(Timeout) and releases every other active bond
    // on the order (the Phase 1 cancel path).
    let resolution = match side {
        Side::Buyer => BondResolution {
            slash_seller: false,
            slash_buyer: true,
        },
        Side::Seller => BondResolution {
            slash_seller: true,
            slash_buyer: false,
        },
    };
    apply_bond_resolution(
        pool,
        ln_client,
        order,
        &resolution,
        BondSlashReason::Timeout,
    )
    .await?;

    // Confirm the slash actually landed before claiming it: a transient
    // settle failure leaves the bond `Locked` (apply_bond_resolution is
    // best-effort), and we must never tell a user their bond was forfeited
    // while the HTLC is still theirs.
    if bond_is_pending_payout(pool, responsible.id).await? {
        info!(
            bond_id = %responsible.id,
            order_id = %order.id,
            role = %responsible.role,
            "Bond slashed on waiting-state timeout"
        );
        Ok(Some(responsible))
    } else {
        warn!(
            bond_id = %responsible.id,
            order_id = %order.id,
            "timeout slash did not land (bond not in PendingPayout); no forfeiture notice sent"
        );
        Ok(None)
    }
}

/// Re-read a bond row's state and report whether it reached
/// `PendingPayout`. Used to confirm a timeout slash truly landed before
/// notifying the slashed user.
async fn bond_is_pending_payout(pool: &Pool<Sqlite>, bond_id: Uuid) -> Result<bool, MostroError> {
    let row: Option<(String,)> = sqlx::query_as("SELECT state FROM bonds WHERE id = ?")
        .bind(bond_id)
        .fetch_optional(pool)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
    Ok(row
        .map(|(s,)| s == BondState::PendingPayout.to_string())
        .unwrap_or(false))
}

/// Phase 4 — best-effort forfeiture notice to the slashed user
/// (`Action::BondSlashed`). Mirrors the Phase 3.5 payout acks: a dropped
/// message must never roll back the slash, so failures are logged, not
/// propagated. The slashed user also receives the order's
/// `Action::Canceled` from the scheduler's normal cancel notification;
/// this message is the bond-specific complement explaining the
/// forfeiture. The `SmallOrder` carries the slashed bond amount in
/// `amount` so the client can render the figure in the user's locale.
pub async fn notify_bond_slashed(order: &Order, slashed: &Bond) {
    let recipient = match PublicKey::from_str(&slashed.pubkey) {
        Ok(pk) => pk,
        Err(e) => {
            warn!(
                bond_id = %slashed.id,
                order_id = %order.id,
                "bond slash: unparseable bonded pubkey ({e}); skipping BondSlashed notice"
            );
            return;
        }
    };
    let order_kind = match order.get_order_kind() {
        Ok(k) => k,
        Err(e) => {
            warn!(
                order_id = %order.id,
                "bond slash: cannot resolve order kind ({e:?}); skipping BondSlashed notice"
            );
            return;
        }
    };
    let small = SmallOrder::new(
        Some(order.id),
        Some(order_kind),
        None,
        slashed.amount_sats,
        order.fiat_code.clone(),
        order.min_amount,
        order.max_amount,
        order.fiat_amount,
        order.payment_method.clone(),
        order.premium,
        None,
        None,
        None,
        None,
        None,
    );
    enqueue_order_msg(
        None,
        Some(order.id),
        Action::BondSlashed,
        Some(Payload::Order(small)),
        recipient,
        None,
    )
    .await;
}

/// Single-bond slash: settle the hold invoice into Mostro's wallet,
/// then CAS the row `Locked → PendingPayout` with snapshot fields.
///
/// Settling first means a transient LND failure leaves the bond
/// retryably `Locked` rather than stranding a `PendingPayout` row
/// whose HTLC is still encumbered. The CAS itself uses
/// `WHERE id = ? AND state = 'locked'` so a duplicate admin call or a
/// concurrent transition cannot overwrite a row that already moved on.
///
/// The CAS write is the only place `slashed_reason`, `slashed_at`, and
/// `node_share_sats` are populated for a `LostDispute` row, which is
/// what makes the split snapshot deterministic across restarts and
/// config changes.
async fn slash_one<L: SettleLightning + Send>(
    pool: &Pool<Sqlite>,
    ln_client: &mut L,
    bond: &Bond,
    reason: BondSlashReason,
    node_share_pct: f64,
) {
    let preimage = match bond.preimage.as_deref() {
        Some(p) => p,
        None => {
            warn!(
                bond_id = %bond.id,
                order_id = %bond.order_id,
                "slash: bond has no preimage — cannot settle HTLC; leaving Locked for operator review"
            );
            return;
        }
    };

    if let Err(e) = ln_client.settle_hold_invoice(preimage).await {
        if is_already_settled_error(&e) {
            info!(
                bond_id = %bond.id,
                order_id = %bond.order_id,
                "slash: HTLC already settled (idempotent retry); proceeding to CAS"
            );
        } else {
            warn!(
                bond_id = %bond.id,
                order_id = %bond.order_id,
                "slash: settle_hold_invoice failed: {e} — leaving bond Locked for admin retry"
            );
            return;
        }
    }

    let node_share_sats = compute_node_share(bond.amount_sats, node_share_pct);
    let now = Utc::now().timestamp();
    let result = sqlx::query(
        "UPDATE bonds \
           SET state = ?, slashed_reason = ?, slashed_at = ?, node_share_sats = ? \
         WHERE id = ? AND state = ?",
    )
    .bind(BondState::PendingPayout.to_string())
    .bind(reason.to_string())
    .bind(now)
    .bind(node_share_sats)
    .bind(bond.id)
    .bind(BondState::Locked.to_string())
    .execute(pool)
    .await;
    match result {
        Ok(r) if r.rows_affected() == 1 => {
            info!(
                bond_id = %bond.id,
                order_id = %bond.order_id,
                reason = %reason,
                node_share_sats,
                counterparty_share_sats = bond.amount_sats - node_share_sats,
                "Bond HTLC settled and row transitioned to PendingPayout"
            );
        }
        Ok(_) => {
            // The bond moved out of `Locked` between our enumerate and
            // our CAS. HTLC is settled either way; whatever path owned
            // the prior transition is responsible for any further
            // movement. Phase 3 will not pick up anything but a
            // `PendingPayout` row.
            warn!(
                bond_id = %bond.id,
                order_id = %bond.order_id,
                current_state = %bond.state,
                "slash CAS no-op (bond state changed concurrently); HTLC was settled"
            );
        }
        Err(e) => {
            warn!(
                bond_id = %bond.id,
                order_id = %bond.order_id,
                "slash CAS DB error: {} (HTLC was settled)", e
            );
        }
    }
}

/// Resolve a buyer/seller slash flag to the matching `Locked` bond row,
/// if any. The mapping uses the §3.1 buyer-side → trade-pubkey lookup
/// on the order, then filters bonds by `pubkey` and `state = Locked`.
///
/// Returns `None` when the side has no `Locked` bond row — either no
/// bond exists on this order for that pubkey, the bond already moved
/// out of `Locked` (e.g. into `Released` or `PendingPayout`), or the
/// side's pubkey is unset on the order. Validation treats `None` as
/// "InvalidPayload"; `apply` treats it as a benign skip.
fn resolve_locked_bond<'a>(order: &Order, bonds: &'a [Bond], side: Side) -> Option<&'a Bond> {
    let target_pubkey = match side {
        Side::Seller => order.seller_pubkey.as_deref()?,
        Side::Buyer => order.buyer_pubkey.as_deref()?,
    };
    let locked = BondState::Locked.to_string();
    bonds
        .iter()
        .find(|b| b.pubkey == target_pubkey && b.state == locked)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use mostro_core::error::{MostroError, ServiceError};
    use mostro_core::message::{Action, Message, Payload};
    use mostro_core::order::{Kind, Order, Status};
    use sqlx::sqlite::SqlitePoolOptions;
    use sqlx::{Pool, Sqlite};

    use super::*;
    use crate::app::bond::model::Bond;
    use crate::app::bond::types::{BondRole, BondSlashReason, BondState};

    /// Recording stub for `SettleLightning`. Captures each preimage the
    /// caller asked LND to settle, and can be primed to return either
    /// success, an "already settled" error, or a transient transport
    /// failure. Used end-to-end to verify the slash path settles at
    /// slash time (one HTLC per slashed bond) and that it skips
    /// non-slashed bonds entirely.
    #[derive(Default)]
    struct StubSettle {
        calls: Mutex<Vec<String>>,
        // When set, force every settle to return this canned error.
        fail_with: Mutex<Option<String>>,
    }

    impl StubSettle {
        fn new() -> Arc<Self> {
            Arc::new(Self::default())
        }
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
        fn fail_next_with(&self, msg: &str) {
            *self.fail_with.lock().unwrap() = Some(msg.to_string());
        }
    }

    impl SettleLightning for Arc<StubSettle> {
        fn settle_hold_invoice<'a>(
            &'a mut self,
            preimage: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<(), MostroError>> + Send + 'a>> {
            Box::pin(async move {
                self.calls.lock().unwrap().push(preimage.to_string());
                if let Some(msg) = self.fail_with.lock().unwrap().take() {
                    return Err(MostroError::MostroInternalErr(ServiceError::LnNodeError(
                        msg,
                    )));
                }
                Ok(())
            })
        }
    }

    async fn setup_pool() -> Pool<Sqlite> {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect(":memory:")
            .await
            .expect("open in-memory sqlite");
        sqlx::query(include_str!(
            "../../../migrations/20221222153301_orders.sql"
        ))
        .execute(&pool)
        .await
        .expect("orders migration");
        for stmt in include_str!("../../../migrations/20251126120000_dev_fee.sql")
            .split(';')
            .map(str::trim)
            .filter(|s| !s.is_empty() && !s.lines().all(|l| l.trim_start().starts_with("--")))
        {
            sqlx::query(stmt)
                .execute(&pool)
                .await
                .expect("dev_fee migration");
        }
        sqlx::query(include_str!(
            "../../../migrations/20260423120000_anti_abuse_bond.sql"
        ))
        .execute(&pool)
        .await
        .expect("bonds migration");
        sqlx::query(include_str!(
            "../../../migrations/20260518120000_bond_payout_payment_hash.sql"
        ))
        .execute(&pool)
        .await
        .expect("bond_payout_payment_hash migration");
        pool
    }

    async fn insert_order_row(pool: &Pool<Sqlite>, order: &Order) {
        sqlx::query(
            r#"INSERT INTO orders (
                id, kind, event_id, status, premium, payment_method,
                amount, fiat_code, fiat_amount, created_at, expires_at,
                seller_pubkey, buyer_pubkey
            ) VALUES (?, ?, ?, ?, 0, ?, ?, ?, ?, ?, ?, ?, ?)"#,
        )
        .bind(order.id)
        .bind(&order.kind)
        .bind(order.id.simple().to_string())
        .bind(&order.status)
        .bind(&order.payment_method)
        .bind(order.amount)
        .bind(&order.fiat_code)
        .bind(order.fiat_amount)
        .bind(order.created_at)
        .bind(order.expires_at)
        .bind(order.seller_pubkey.as_deref())
        .bind(order.buyer_pubkey.as_deref())
        .execute(pool)
        .await
        .expect("insert order");
    }

    fn fixture_order(kind: Kind, seller_pk: &str, buyer_pk: &str) -> Order {
        Order {
            id: Uuid::new_v4(),
            kind: kind.to_string(),
            status: Status::Dispute.to_string(),
            seller_pubkey: Some(seller_pk.to_string()),
            buyer_pubkey: Some(buyer_pk.to_string()),
            amount: 100_000,
            fiat_code: "USD".to_string(),
            fiat_amount: 10,
            payment_method: "lightning".to_string(),
            created_at: Utc::now().timestamp(),
            expires_at: Utc::now().timestamp() + 3600,
            ..Order::default()
        }
    }

    /// 32-byte zero preimage as a 64-char hex string. Real bonds carry
    /// a random preimage populated by `request_taker_bond`; tests just
    /// need *something* well-formed for the stub `SettleLightning` to
    /// observe.
    fn stub_preimage() -> String {
        "00".repeat(32)
    }

    async fn insert_bond(
        pool: &Pool<Sqlite>,
        order_id: Uuid,
        pubkey: &str,
        state: BondState,
    ) -> Bond {
        let mut b = Bond::new_requested(order_id, pubkey.to_string(), BondRole::Taker, 10_000);
        b.state = state.to_string();
        b.preimage = Some(stub_preimage());
        // No hash → release_bond skips the LND cancel branch entirely
        // (see `release_bond` in flow.rs).
        b.hash = None;
        sqlx_crud::Crud::create(b.clone(), pool).await.unwrap();
        b
    }

    fn taker_pk() -> &'static str {
        "1111111111111111111111111111111111111111111111111111111111111111"
    }
    fn maker_pk() -> &'static str {
        "2222222222222222222222222222222222222222222222222222222222222222"
    }

    fn order_msg_with(payload: Option<Payload>) -> Message {
        Message::new_order(
            Some(Uuid::new_v4()),
            None,
            None,
            Action::AdminSettle,
            payload,
        )
    }

    #[test]
    fn extract_returns_default_when_payload_absent() {
        // The pre-Phase-2 admin client sends `payload: None`. The
        // extractor must default to "release all bonds" so Phase 1
        // behaviour is preserved bit-for-bit.
        let msg = order_msg_with(None);
        let br = extract_bond_resolution(&msg);
        assert!(!br.slash_seller);
        assert!(!br.slash_buyer);
    }

    #[test]
    fn extract_returns_default_for_unrelated_payload_shapes() {
        // A payload of the wrong shape is upstream-rejected by verify
        // for AdminSettle/Cancel, but defending here means an exotic
        // future variant cannot accidentally activate a slash.
        let msg = order_msg_with(Some(Payload::TextMessage("hi".into())));
        let br = extract_bond_resolution(&msg);
        assert!(!br.slash_seller);
        assert!(!br.slash_buyer);
    }

    #[test]
    fn extract_returns_payload_when_present() {
        let payload = Payload::BondResolution(BondResolution {
            slash_seller: true,
            slash_buyer: false,
        });
        let msg = order_msg_with(Some(payload));
        let br = extract_bond_resolution(&msg);
        assert!(br.slash_seller);
        assert!(!br.slash_buyer);
    }

    #[tokio::test]
    async fn validate_null_payload_passes_with_no_bonds() {
        // null/false-false payload + no bond rows = legacy Phase 1
        // behaviour. Must pass without touching the DB beyond the
        // (empty) lookup.
        let pool = setup_pool().await;
        let order = fixture_order(Kind::Sell, maker_pk(), taker_pk());
        insert_order_row(&pool, &order).await;
        let res = BondResolution {
            slash_seller: false,
            slash_buyer: false,
        };
        validate_bond_resolution(&pool, &order, &res).await.unwrap();
    }

    #[tokio::test]
    async fn validate_slash_buyer_passes_when_buyer_has_locked_bond() {
        // sell-order: taker is buyer, with a Locked bond.
        let pool = setup_pool().await;
        let order = fixture_order(Kind::Sell, maker_pk(), taker_pk());
        insert_order_row(&pool, &order).await;
        insert_bond(&pool, order.id, taker_pk(), BondState::Locked).await;
        let res = BondResolution {
            slash_seller: false,
            slash_buyer: true,
        };
        validate_bond_resolution(&pool, &order, &res).await.unwrap();
    }

    #[tokio::test]
    async fn validate_slash_seller_on_sell_apply_to_take_rejects() {
        // Spec test: cancel + slash_seller on a sell-order with
        // apply_to=take. The seller is the maker and has no bond (only
        // taker bonds in Phase 2). Must fail before any trade mutation.
        let pool = setup_pool().await;
        let order = fixture_order(Kind::Sell, maker_pk(), taker_pk());
        insert_order_row(&pool, &order).await;
        // Taker has a Locked bond, but the seller (maker) has none.
        insert_bond(&pool, order.id, taker_pk(), BondState::Locked).await;
        let res = BondResolution {
            slash_seller: true,
            slash_buyer: false,
        };
        let err = validate_bond_resolution(&pool, &order, &res)
            .await
            .unwrap_err();
        assert!(
            matches!(err, MostroCantDo(CantDoReason::InvalidPayload)),
            "expected CantDo(InvalidPayload), got {err:?}"
        );
    }

    #[tokio::test]
    async fn validate_rejects_when_bond_table_is_empty() {
        // Feature-disabled-style scenario: no bond rows at all. Any
        // slash flag must be rejected.
        let pool = setup_pool().await;
        let order = fixture_order(Kind::Sell, maker_pk(), taker_pk());
        insert_order_row(&pool, &order).await;
        let res = BondResolution {
            slash_seller: false,
            slash_buyer: true,
        };
        let err = validate_bond_resolution(&pool, &order, &res)
            .await
            .unwrap_err();
        assert!(matches!(err, MostroCantDo(CantDoReason::InvalidPayload)));
    }

    #[tokio::test]
    async fn apply_null_payload_releases_all_active_bonds() {
        // payload=null preserves Phase 1: any active bond on the order
        // is released. Bond table contents are exercised; LND is not
        // touched because the bond has `hash = None`.
        let pool = setup_pool().await;
        let order = fixture_order(Kind::Sell, maker_pk(), taker_pk());
        insert_order_row(&pool, &order).await;
        let bond = insert_bond(&pool, order.id, taker_pk(), BondState::Locked).await;

        let res = BondResolution {
            slash_seller: false,
            slash_buyer: false,
        };
        apply_bond_resolution(
            &pool,
            &mut StubSettle::new(),
            &order,
            &res,
            BondSlashReason::LostDispute,
        )
        .await
        .unwrap();

        let row: (String,) = sqlx::query_as("SELECT state FROM bonds WHERE id = ?")
            .bind(bond.id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            row.0,
            BondState::Released.to_string(),
            "null payload must release, not slash"
        );
    }

    #[tokio::test]
    async fn apply_slash_buyer_on_sell_order_transitions_taker_bond() {
        // Spec example: settle + slash_buyer=true on a sell-order. Taker
        // is the buyer; their Locked bond enters PendingPayout with the
        // split snapshot persisted on the row.
        let pool = setup_pool().await;
        let order = fixture_order(Kind::Sell, maker_pk(), taker_pk());
        insert_order_row(&pool, &order).await;
        let bond = insert_bond(&pool, order.id, taker_pk(), BondState::Locked).await;
        let res = BondResolution {
            slash_seller: false,
            slash_buyer: true,
        };
        apply_bond_resolution(
            &pool,
            &mut StubSettle::new(),
            &order,
            &res,
            BondSlashReason::LostDispute,
        )
        .await
        .unwrap();

        let row: (String, Option<String>, Option<i64>, Option<i64>) = sqlx::query_as(
            "SELECT state, slashed_reason, slashed_at, node_share_sats \
             FROM bonds WHERE id = ?",
        )
        .bind(bond.id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.0, BondState::PendingPayout.to_string());
        assert_eq!(row.1.as_deref(), Some("lost-dispute"));
        assert!(row.2.unwrap() > 0, "slashed_at must be set");
        // No `[anti_abuse_bond]` block in test config → Settings::get_bond
        // returns None and the helper falls back to pct=0.0 (legacy
        // winner-takes-all). The load-bearing assertion is that the
        // snapshot is *persisted* on the row; the specific value is a
        // function of config and is exercised by math.rs tests.
        assert_eq!(row.3, Some(0));
    }

    #[tokio::test]
    async fn apply_is_idempotent_on_already_pending_payout() {
        // A duplicate admin call (or a slash CAS racing with itself)
        // must not rebump `slashed_at` or rewrite `node_share_sats`.
        let pool = setup_pool().await;
        let order = fixture_order(Kind::Sell, maker_pk(), taker_pk());
        insert_order_row(&pool, &order).await;
        let bond = insert_bond(&pool, order.id, taker_pk(), BondState::Locked).await;
        let res = BondResolution {
            slash_seller: false,
            slash_buyer: true,
        };

        apply_bond_resolution(
            &pool,
            &mut StubSettle::new(),
            &order,
            &res,
            BondSlashReason::LostDispute,
        )
        .await
        .unwrap();
        let first: (String, Option<i64>, Option<i64>) =
            sqlx::query_as("SELECT state, slashed_at, node_share_sats FROM bonds WHERE id = ?")
                .bind(bond.id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(first.0, BondState::PendingPayout.to_string());

        // Pretend a duplicate admin message arrived a second later.
        std::thread::sleep(std::time::Duration::from_secs(1));
        apply_bond_resolution(
            &pool,
            &mut StubSettle::new(),
            &order,
            &res,
            BondSlashReason::LostDispute,
        )
        .await
        .unwrap();
        let second: (String, Option<i64>, Option<i64>) =
            sqlx::query_as("SELECT state, slashed_at, node_share_sats FROM bonds WHERE id = ?")
                .bind(bond.id)
                .fetch_one(&pool)
                .await
                .unwrap();
        // The state must not flip back to `Released` or anything else —
        // a regression in `find_active_bonds_for_order`'s state filter
        // could route the row through `release_bond` on the second pass
        // without this assertion catching it.
        assert_eq!(
            second.0,
            BondState::PendingPayout.to_string(),
            "second apply must not transition the bond out of PendingPayout"
        );
        assert_eq!(
            first, second,
            "second apply must not rebump state / slashed_at / node_share_sats"
        );
    }

    #[tokio::test]
    async fn apply_with_no_bond_rows_is_noop() {
        // Feature-disabled-shaped path: bond table is empty. The helper
        // must complete without error and without writing anything.
        let pool = setup_pool().await;
        let order = fixture_order(Kind::Sell, maker_pk(), taker_pk());
        insert_order_row(&pool, &order).await;
        let res = BondResolution {
            slash_seller: false,
            slash_buyer: false,
        };
        apply_bond_resolution(
            &pool,
            &mut StubSettle::new(),
            &order,
            &res,
            BondSlashReason::LostDispute,
        )
        .await
        .unwrap();
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM bonds")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count.0, 0);
    }

    // ── Immediate-settle (the Phase 2 change shipped here) ──────────────────

    #[tokio::test]
    async fn slash_one_settles_exactly_one_htlc() {
        // Single slashed taker bond → `settle_hold_invoice` runs once
        // with that bond's preimage as part of the slash step. The row
        // ends up in `PendingPayout` for Phase 3 to handle the
        // counterparty payout asynchronously.
        let pool = setup_pool().await;
        let order = fixture_order(Kind::Sell, maker_pk(), taker_pk());
        insert_order_row(&pool, &order).await;
        let bond = insert_bond(&pool, order.id, taker_pk(), BondState::Locked).await;
        let mut ln = StubSettle::new();
        let res = BondResolution {
            slash_seller: false,
            slash_buyer: true,
        };

        apply_bond_resolution(&pool, &mut ln, &order, &res, BondSlashReason::LostDispute)
            .await
            .unwrap();

        assert_eq!(
            ln.calls(),
            vec![stub_preimage()],
            "slash path must settle exactly the slashed bond's HTLC"
        );
        let state: (String,) = sqlx::query_as("SELECT state FROM bonds WHERE id = ?")
            .bind(bond.id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(state.0, BondState::PendingPayout.to_string());
    }

    #[tokio::test]
    async fn slash_both_settles_both_htlcs() {
        // Both flags set + both buyer and seller carry a `Locked` bond:
        // `settle_hold_invoice` must run **twice** (once per slashed
        // bond) and both rows end up in `PendingPayout`. This is the
        // Phase 5+ "both behaved badly" path; Phase 2 cannot reach it
        // in production (taker-only world), but the slash machinery
        // must handle the case correctly when Phase 5 wires it.
        let pool = setup_pool().await;
        let order = fixture_order(Kind::Sell, maker_pk(), taker_pk());
        insert_order_row(&pool, &order).await;
        let buyer_bond = insert_bond(&pool, order.id, taker_pk(), BondState::Locked).await;
        let seller_bond = insert_bond(&pool, order.id, maker_pk(), BondState::Locked).await;
        let mut ln = StubSettle::new();
        let res = BondResolution {
            slash_seller: true,
            slash_buyer: true,
        };

        apply_bond_resolution(&pool, &mut ln, &order, &res, BondSlashReason::LostDispute)
            .await
            .unwrap();

        // Both preimages observed (order-independent: the apply loop
        // walks `find_active_bonds_for_order` results which are not
        // ordered by side).
        assert_eq!(
            ln.calls().len(),
            2,
            "both slashed bonds must have their HTLCs settled immediately"
        );
        for b in [&buyer_bond, &seller_bond] {
            let state: (String,) = sqlx::query_as("SELECT state FROM bonds WHERE id = ?")
                .bind(b.id)
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(state.0, BondState::PendingPayout.to_string());
        }
    }

    #[tokio::test]
    async fn non_slashed_bond_is_released_not_settled() {
        // When the resolution releases (no flags set), the slash path
        // must NOT touch `settle_hold_invoice` — release is the
        // Phase 1 `cancel_hold_invoice` flow.
        let pool = setup_pool().await;
        let order = fixture_order(Kind::Sell, maker_pk(), taker_pk());
        insert_order_row(&pool, &order).await;
        let bond = insert_bond(&pool, order.id, taker_pk(), BondState::Locked).await;
        let mut ln = StubSettle::new();
        let res = BondResolution {
            slash_seller: false,
            slash_buyer: false,
        };

        apply_bond_resolution(&pool, &mut ln, &order, &res, BondSlashReason::LostDispute)
            .await
            .unwrap();

        assert!(
            ln.calls().is_empty(),
            "non-slashed (released) bonds must not invoke settle_hold_invoice"
        );
        let state: (String,) = sqlx::query_as("SELECT state FROM bonds WHERE id = ?")
            .bind(bond.id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(state.0, BondState::Released.to_string());
    }

    #[tokio::test]
    async fn slash_treats_already_settled_as_success() {
        // Admin retry: the HTLC was claimed on a previous attempt but
        // the CAS failed. LND returns "already settled"; the slash
        // path must classify that as success via
        // `is_already_settled_error` and complete the CAS to
        // `PendingPayout`.
        let pool = setup_pool().await;
        let order = fixture_order(Kind::Sell, maker_pk(), taker_pk());
        insert_order_row(&pool, &order).await;
        let bond = insert_bond(&pool, order.id, taker_pk(), BondState::Locked).await;
        let mut ln = StubSettle::new();
        ln.fail_next_with("code=AlreadyExists: invoice already settled");
        let res = BondResolution {
            slash_seller: false,
            slash_buyer: true,
        };

        apply_bond_resolution(&pool, &mut ln, &order, &res, BondSlashReason::LostDispute)
            .await
            .unwrap();

        let state: (String,) = sqlx::query_as("SELECT state FROM bonds WHERE id = ?")
            .bind(bond.id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            state.0,
            BondState::PendingPayout.to_string(),
            "already-settled error must not block the CAS"
        );
    }

    #[tokio::test]
    async fn slash_settle_transport_failure_leaves_bond_locked() {
        // Real LND transport failure: the bond stays `Locked` so a
        // future admin retry can re-attempt the slash. The CAS must
        // NOT have flipped the row to `PendingPayout`.
        let pool = setup_pool().await;
        let order = fixture_order(Kind::Sell, maker_pk(), taker_pk());
        insert_order_row(&pool, &order).await;
        let bond = insert_bond(&pool, order.id, taker_pk(), BondState::Locked).await;
        let mut ln = StubSettle::new();
        ln.fail_next_with("code=Unavailable: connection refused");
        let res = BondResolution {
            slash_seller: false,
            slash_buyer: true,
        };

        apply_bond_resolution(&pool, &mut ln, &order, &res, BondSlashReason::LostDispute)
            .await
            .unwrap();

        let state: (String,) = sqlx::query_as("SELECT state FROM bonds WHERE id = ?")
            .bind(bond.id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            state.0,
            BondState::Locked.to_string(),
            "transient settle failure must leave the bond Locked for admin retry"
        );
    }

    // ── Phase 4 — timeout-slash dispatch ────────────────────────────────────

    use crate::config::types::{AntiAbuseBondSettings, BondApplyTo};

    /// Bond config for the Phase 4 gate. `Default` is `enabled = false`;
    /// override only the three flags `slash_or_release_on_timeout`
    /// inspects.
    fn timeout_cfg(
        enabled: bool,
        slash_on_waiting_timeout: bool,
        apply_to: BondApplyTo,
    ) -> AntiAbuseBondSettings {
        AntiAbuseBondSettings {
            enabled,
            slash_on_waiting_timeout,
            apply_to,
            ..AntiAbuseBondSettings::default()
        }
    }

    /// `fixture_order` parks the order in `Dispute`; the Phase 4 dispatch
    /// keys off the waiting state, so override it.
    fn waiting_order(kind: Kind, seller_pk: &str, buyer_pk: &str, status: Status) -> Order {
        let mut o = fixture_order(kind, seller_pk, buyer_pk);
        o.status = status.to_string();
        o
    }

    async fn read_bond_state(pool: &Pool<Sqlite>, id: Uuid) -> String {
        let row: (String,) = sqlx::query_as("SELECT state FROM bonds WHERE id = ?")
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap();
        row.0
    }

    #[tokio::test]
    async fn timeout_slash_disabled_releases_without_slashing() {
        // slash_on_waiting_timeout = false: even with the responsible
        // taker holding a Locked bond on a timed-out waiting state, the
        // bond is released (Phase 1 behaviour), never slashed, and the
        // HTLC is never settled.
        let pool = setup_pool().await;
        let order = waiting_order(
            Kind::Sell,
            maker_pk(),
            taker_pk(),
            Status::WaitingBuyerInvoice,
        );
        insert_order_row(&pool, &order).await;
        let bond = insert_bond(&pool, order.id, taker_pk(), BondState::Locked).await;
        let mut ln = StubSettle::new();

        let cfg = timeout_cfg(true, false, BondApplyTo::Take);
        let result = slash_or_release_on_timeout(&pool, &mut ln, &order, Some(&cfg))
            .await
            .unwrap();

        assert!(
            result.is_none(),
            "disabled timeout slash must not report a slash"
        );
        assert!(
            ln.calls().is_empty(),
            "release path must not settle the HTLC"
        );
        assert_eq!(
            read_bond_state(&pool, bond.id).await,
            BondState::Released.to_string()
        );
    }

    #[tokio::test]
    async fn timeout_slash_sell_buyer_silent_slashes_taker_bond() {
        // sell order, WaitingBuyerInvoice: the buyer is responsible and on
        // a sell order the buyer is the taker. Gate armed → the taker bond
        // is slashed with reason=Timeout and the HTLC is settled exactly
        // once. The dispatch reports the slashed bond.
        let pool = setup_pool().await;
        let order = waiting_order(
            Kind::Sell,
            maker_pk(),
            taker_pk(),
            Status::WaitingBuyerInvoice,
        );
        insert_order_row(&pool, &order).await;
        let bond = insert_bond(&pool, order.id, taker_pk(), BondState::Locked).await;
        let mut ln = StubSettle::new();

        let cfg = timeout_cfg(true, true, BondApplyTo::Take);
        let result = slash_or_release_on_timeout(&pool, &mut ln, &order, Some(&cfg))
            .await
            .unwrap();

        assert_eq!(
            result.map(|b| b.id),
            Some(bond.id),
            "must report the slashed bond for notification"
        );
        assert_eq!(
            ln.calls(),
            vec![stub_preimage()],
            "slash settles exactly the responsible bond's HTLC"
        );
        let row: (String, Option<String>, Option<i64>) =
            sqlx::query_as("SELECT state, slashed_reason, slashed_at FROM bonds WHERE id = ?")
                .bind(bond.id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(row.0, BondState::PendingPayout.to_string());
        assert_eq!(row.1.as_deref(), Some("timeout"));
        assert!(row.2.unwrap() > 0, "slashed_at must be set");
    }

    #[tokio::test]
    async fn timeout_slash_buy_seller_silent_slashes_taker_bond() {
        // buy order, WaitingPayment: the seller is responsible and on a
        // buy order the seller is the taker. (For a buy order the maker is
        // the buyer, so the taker's trade pubkey lives in `seller_pubkey`.)
        // Gate armed → taker bond slashed with reason=Timeout.
        let pool = setup_pool().await;
        let order = waiting_order(Kind::Buy, taker_pk(), maker_pk(), Status::WaitingPayment);
        insert_order_row(&pool, &order).await;
        let bond = insert_bond(&pool, order.id, taker_pk(), BondState::Locked).await;
        let mut ln = StubSettle::new();

        let cfg = timeout_cfg(true, true, BondApplyTo::Take);
        let result = slash_or_release_on_timeout(&pool, &mut ln, &order, Some(&cfg))
            .await
            .unwrap();

        assert_eq!(result.map(|b| b.id), Some(bond.id));
        assert_eq!(ln.calls(), vec![stub_preimage()]);
        assert_eq!(
            read_bond_state(&pool, bond.id).await,
            BondState::PendingPayout.to_string()
        );
    }

    #[tokio::test]
    async fn timeout_no_slash_when_responsible_party_is_maker_sell_order() {
        // sell order, WaitingPayment: the seller is responsible, and on a
        // sell order the seller is the *maker*, who holds no bond under
        // apply_to=take. The taker (buyer) bond exists but belongs to the
        // non-responsible party — it must be released, never slashed.
        // This is the load-bearing money-safety case: a counterparty
        // going silent must not cost the *other* party their bond.
        let pool = setup_pool().await;
        let order = waiting_order(Kind::Sell, maker_pk(), taker_pk(), Status::WaitingPayment);
        insert_order_row(&pool, &order).await;
        let bond = insert_bond(&pool, order.id, taker_pk(), BondState::Locked).await;
        let mut ln = StubSettle::new();

        let cfg = timeout_cfg(true, true, BondApplyTo::Take);
        let result = slash_or_release_on_timeout(&pool, &mut ln, &order, Some(&cfg))
            .await
            .unwrap();

        assert!(
            result.is_none(),
            "maker-responsible row must not slash the taker's bond"
        );
        assert!(ln.calls().is_empty());
        assert_eq!(
            read_bond_state(&pool, bond.id).await,
            BondState::Released.to_string()
        );
    }

    #[tokio::test]
    async fn timeout_no_slash_when_responsible_party_is_maker_buy_order() {
        // buy order, WaitingBuyerInvoice: the buyer is responsible, and on
        // a buy order the buyer is the maker (no bond under apply_to=take).
        // The taker (seller) bond is released, not slashed.
        let pool = setup_pool().await;
        let order = waiting_order(
            Kind::Buy,
            taker_pk(),
            maker_pk(),
            Status::WaitingBuyerInvoice,
        );
        insert_order_row(&pool, &order).await;
        let bond = insert_bond(&pool, order.id, taker_pk(), BondState::Locked).await;
        let mut ln = StubSettle::new();

        let cfg = timeout_cfg(true, true, BondApplyTo::Take);
        let result = slash_or_release_on_timeout(&pool, &mut ln, &order, Some(&cfg))
            .await
            .unwrap();

        assert!(result.is_none());
        assert!(ln.calls().is_empty());
        assert_eq!(
            read_bond_state(&pool, bond.id).await,
            BondState::Released.to_string()
        );
    }

    #[tokio::test]
    async fn timeout_slash_skipped_when_apply_to_make_only() {
        // apply_to=make: the taker side posts no bond, so the timeout path
        // must not slash a taker bond even with an otherwise-armed config.
        let pool = setup_pool().await;
        let order = waiting_order(
            Kind::Sell,
            maker_pk(),
            taker_pk(),
            Status::WaitingBuyerInvoice,
        );
        insert_order_row(&pool, &order).await;
        let bond = insert_bond(&pool, order.id, taker_pk(), BondState::Locked).await;
        let mut ln = StubSettle::new();

        let cfg = timeout_cfg(true, true, BondApplyTo::Make);
        let result = slash_or_release_on_timeout(&pool, &mut ln, &order, Some(&cfg))
            .await
            .unwrap();

        assert!(result.is_none());
        assert!(ln.calls().is_empty());
        assert_eq!(
            read_bond_state(&pool, bond.id).await,
            BondState::Released.to_string()
        );
    }

    #[tokio::test]
    async fn timeout_no_config_releases_bond() {
        // No [anti_abuse_bond] block (cfg = None): the dispatch still
        // drains any active bond (release), and never slashes. This is the
        // path that cleans up bonds left over from a previously-enabled
        // period.
        let pool = setup_pool().await;
        let order = waiting_order(
            Kind::Sell,
            maker_pk(),
            taker_pk(),
            Status::WaitingBuyerInvoice,
        );
        insert_order_row(&pool, &order).await;
        let bond = insert_bond(&pool, order.id, taker_pk(), BondState::Locked).await;
        let mut ln = StubSettle::new();

        let result = slash_or_release_on_timeout(&pool, &mut ln, &order, None)
            .await
            .unwrap();

        assert!(result.is_none());
        assert!(ln.calls().is_empty());
        assert_eq!(
            read_bond_state(&pool, bond.id).await,
            BondState::Released.to_string()
        );
    }

    #[tokio::test]
    async fn timeout_slash_transient_settle_failure_leaves_bond_locked_and_no_notice() {
        // A transient settle failure leaves the bond Locked (the apply
        // primitive is best-effort). The dispatch must re-read the row and
        // report None, so the caller never sends a forfeiture notice for a
        // bond whose HTLC is still the user's.
        let pool = setup_pool().await;
        let order = waiting_order(
            Kind::Sell,
            maker_pk(),
            taker_pk(),
            Status::WaitingBuyerInvoice,
        );
        insert_order_row(&pool, &order).await;
        let bond = insert_bond(&pool, order.id, taker_pk(), BondState::Locked).await;
        let mut ln = StubSettle::new();
        ln.fail_next_with("code=Unavailable: connection refused");

        let cfg = timeout_cfg(true, true, BondApplyTo::Take);
        let result = slash_or_release_on_timeout(&pool, &mut ln, &order, Some(&cfg))
            .await
            .unwrap();

        assert!(
            result.is_none(),
            "an unconfirmed slash must not be reported to the caller"
        );
        assert_eq!(
            read_bond_state(&pool, bond.id).await,
            BondState::Locked.to_string(),
            "transient settle failure must leave the bond Locked for retry"
        );
    }

    #[tokio::test]
    async fn notify_bond_slashed_targets_the_slashed_user() {
        // The forfeiture notice goes to the bonded (slashed) taker,
        // carrying Action::BondSlashed scoped to the order.
        use crate::config::MESSAGE_QUEUES;
        let pool = setup_pool().await;
        let order = waiting_order(
            Kind::Sell,
            maker_pk(),
            taker_pk(),
            Status::WaitingBuyerInvoice,
        );
        insert_order_row(&pool, &order).await;
        let bond = insert_bond(&pool, order.id, taker_pk(), BondState::PendingPayout).await;

        notify_bond_slashed(&order, &bond).await;

        let recipients: Vec<String> = MESSAGE_QUEUES
            .queue_order_msg
            .read()
            .await
            .iter()
            .filter(|(m, _)| {
                let k = m.get_inner_message_kind();
                k.id == Some(order.id) && k.action == Action::BondSlashed
            })
            .map(|(_, pk)| pk.to_string())
            .collect();
        assert_eq!(
            recipients,
            vec![taker_pk().to_string()],
            "BondSlashed must be enqueued to the slashed taker only"
        );
    }
}
