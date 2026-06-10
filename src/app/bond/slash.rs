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
use mostro_core::order::{Kind, Order, SmallOrder, Status};
use nostr_sdk::prelude::PublicKey;
use sqlx::{Pool, Sqlite};
use tracing::{info, warn};
use uuid::Uuid;

use super::db::{
    create_bond, find_active_bonds_for_order, find_child_slashes_for_parent,
    find_maker_bond_for_order, find_range_root_order,
};
use super::flow::{
    release_bond, release_bonds_for_order_or_warn, release_taker_bonds_for_order_or_warn,
};
use super::math::compute_node_share;
use super::model::Bond;
use super::types::{BondRole, BondSlashReason, BondState};
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
    let is_range = order_has_range_maker_bond(pool, order).await?;
    if resolution.slash_seller
        && resolve_slash_target(pool, order, &bonds, Side::Seller, is_range)
            .await?
            .is_none()
    {
        return Err(MostroCantDo(CantDoReason::InvalidPayload));
    }
    if resolution.slash_buyer
        && resolve_slash_target(pool, order, &bonds, Side::Buyer, is_range)
            .await?
            .is_none()
    {
        return Err(MostroCantDo(CantDoReason::InvalidPayload));
    }
    Ok(())
}

/// Is the bonded party on `side` the **maker** (vs the taker) for this
/// order? §3.1: a `sell` order's maker is the seller; a `buy` order's
/// maker is the buyer.
fn side_is_maker(order: &Order, side: Side) -> Result<bool, MostroError> {
    let kind = order.get_order_kind().map_err(MostroInternalErr)?;
    Ok(matches!(
        (kind, side),
        (Kind::Sell, Side::Seller) | (Kind::Buy, Side::Buyer)
    ))
}

/// Resolve the `Locked` bond a slash flag targets, owned so callers don't
/// juggle borrow lifetimes across the range-root fallback.
///
/// Resolution order:
/// 1. **Pubkey match on this order's active bonds** ([`resolve_locked_bond`]
///    via the §3.1 buyer/seller → trade-pubkey lookup). This covers taker
///    bonds and a non-range / first-slice maker bond (which lives on the
///    order itself).
/// 2. **Range-root fallback** — only for the *maker* side of a range order
///    whose slash landed on a descendant slice (the maker bond lives on the
///    range root, not the slice). Walks `range_parent_id` via
///    [`find_maker_bond_for_order`].
async fn resolve_slash_target(
    pool: &Pool<Sqlite>,
    order: &Order,
    bonds: &[Bond],
    side: Side,
    is_range: bool,
) -> Result<Option<Bond>, MostroError> {
    if let Some(b) = resolve_locked_bond(order, bonds, side) {
        return Ok(Some(b.clone()));
    }
    if is_range && side_is_maker(order, side)? {
        let locked = BondState::Locked.to_string();
        if let Some(b) = find_maker_bond_for_order(pool, order).await? {
            if b.state == locked {
                return Ok(Some(b));
            }
        }
    }
    Ok(None)
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
    // Active bonds attached to *this* order — i.e. the taker bond(s) on
    // this slice. The maker bond may live on a range root elsewhere and is
    // resolved separately via `find_maker_bond_for_order`.
    let active = find_active_bonds_for_order(pool, order.id).await?;

    // Snapshot the split percentage *once* per call. Phase 3 will read
    // `node_share_sats` off each row; we never recompute it after the
    // transition.
    let node_share_pct = Settings::get_bond().map_or(0.0, |c| c.slash_node_share_pct);

    // Is the maker bond governing this order a *range* bond (one HTLC sized
    // against `max_amount`, slashed proportionally per slice and settled
    // only at range close)? If so, the maker bond must never be settled or
    // released inline here — `resolve_range_maker_bond_at_close` owns its
    // HTLC. We still record a proportional child slash row below.
    let is_range = order_has_range_maker_bond(pool, order).await?;

    // Track the bonds we slashed inline (via `slash_one`) so the release
    // sweep at the end skips them. Range maker slashes don't go here — the
    // parent HTLC stays `Locked` and is excluded from the sweep by the
    // `is_range` guard instead.
    let mut slashed_ids: HashSet<Uuid> = HashSet::new();

    for (flag, side) in [
        (resolution.slash_seller, Side::Seller),
        (resolution.slash_buyer, Side::Buyer),
    ] {
        if !flag {
            continue;
        }
        // No-op if no Locked bond resolves: validation should have run
        // before any trade-side mutation. Reaching here with a missing bond
        // means a concurrent path raced between validate and apply — the
        // release sweep below handles whatever remains safely.
        let Some(target) = resolve_slash_target(pool, order, &active, side, is_range).await? else {
            continue;
        };
        if is_range && side_is_maker(order, side)? {
            // Phase 6: the maker bond on a range order is slashed
            // proportionally per slice — record a child row and leave the
            // parent HTLC `Locked`. The single settle happens at range close
            // (`resolve_range_maker_bond_at_close`, called by the admin
            // handler right after this returns).
            let root = find_range_root_order(pool, order.clone()).await?;
            record_maker_slice_slash(pool, order, &root, &target, reason, node_share_pct).await?;
        } else {
            // Taker bond, or a non-range maker bond (Phase 2/5): settle the
            // HTLC inline.
            slash_one(pool, ln_client, &target, reason, node_share_pct).await;
            slashed_ids.insert(target.id);
        }
    }

    // Release the non-slashed bonds attached to this order. For a range
    // order the maker bond is deliberately retained: its HTLC spans the
    // whole range and is resolved (settled-at-close or released) by
    // `resolve_range_maker_bond_at_close` once the range terminates — the
    // admin handler calls that right after this function returns.
    let maker_role = BondRole::Maker.to_string();
    for bond in active.iter() {
        if slashed_ids.contains(&bond.id) {
            continue;
        }
        if is_range && bond.role == maker_role {
            continue;
        }
        if let Err(e) = release_bond(pool, bond).await {
            warn!(
                bond_id = %bond.id,
                order_id = %order.id,
                "apply_bond_resolution: release_bond failed: {}", e
            );
        }
    }

    Ok(())
}

/// True when the maker bond governing `order` is a **range** bond — i.e.
/// the order's range root carries a `max_amount`. Range maker bonds use
/// the Phase 6 accumulate-and-settle-at-close path; everything else uses
/// the Phase 2/5 inline settle.
async fn order_has_range_maker_bond(
    pool: &Pool<Sqlite>,
    order: &Order,
) -> Result<bool, MostroError> {
    let root = find_range_root_order(pool, order.clone()).await?;
    Ok(root.max_amount.is_some())
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
/// slash actually landed, so the caller can notify the slashed user;
/// `Ok(None)` otherwise. The slash is confirmed by re-reading the row's
/// durable `slashed_reason = Timeout` metadata (see
/// [`timeout_slash_confirmed`]) rather than a transient
/// `state = PendingPayout` check — the concurrent payout scheduler can
/// move a just-slashed row onward within the confirmation window.
/// [`apply_bond_resolution`] is best-effort and leaves the bond `Locked`
/// (no slash metadata) on a transient settle failure, so a `Some` return
/// guarantees the bond was really forfeited and the notice is truthful.
///
/// `order` MUST carry the pre-cancel waiting status and the trade
/// pubkeys; call this from the persist-success branch that replaces the
/// Phase 1 release call. `bond_cfg` is the node's `[anti_abuse_bond]`
/// config (the scheduler passes `Settings::get_bond()`); it is taken as a
/// parameter rather than read from the global so the gate is unit-testable
/// without mutating process-wide state.
/// Does a waiting-state timeout on this order **republish** it (return it
/// to the book in `Pending`) rather than cancel it outright?
///
/// Mirrors the republish branch of `scheduler::job_cancel_orders`:
/// `(WaitingBuyerInvoice, Sell)` and `(WaitingPayment, Buy)` republish —
/// in both the responsible party is the *taker*, so the maker stays
/// committed and the order goes back to the book. The complementary
/// `(WaitingBuyerInvoice, Buy)` / `(WaitingPayment, Sell)` cases cancel the
/// order (the responsible party is the *maker*). Keep this in sync with the
/// scheduler's match.
fn order_republishes_on_timeout(order: &Order) -> bool {
    matches!(
        (order.get_order_status(), order.get_order_kind()),
        (Ok(Status::WaitingBuyerInvoice), Ok(Kind::Sell))
            | (Ok(Status::WaitingPayment), Ok(Kind::Buy))
    )
}

/// Release the still-active bonds on a timed-out order, honouring the
/// republish-vs-cancel distinction: on a republish the maker's `Locked`
/// bond is retained (it follows the order's lifecycle), otherwise every
/// bond is released.
async fn release_on_timeout(pool: &Pool<Sqlite>, order_id: Uuid, republishes: bool) {
    if republishes {
        release_taker_bonds_for_order_or_warn(pool, order_id, "scheduler_timeout").await;
    } else {
        release_bonds_for_order_or_warn(pool, order_id, "scheduler_timeout").await;
    }
}

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

    // Will this timeout **republish** the order (return it to the book) or
    // **terminate** it? When the taker is the responsible party the order
    // goes back to `Pending` and is republished, so the maker's commitment
    // survives — its bond stays `Locked` and is resolved only when the
    // order itself terminates (completed / cancelled / `Pending` expiry).
    // Only the abandoning taker side is settled here. When the maker is
    // responsible the order is cancelled outright, so every bond is
    // released. This mirrors `scheduler::job_cancel_orders`' own
    // republish-vs-cancel split (keep the two in sync).
    let republishes = order_republishes_on_timeout(order);

    // Gate the slash. `apply_to` is a posting-timing switch; Phase 4 is
    // taker-only, so we check `applies_to_taker` (Phase 7 widens this to
    // the maker). When the gate is closed we still release — bonds left
    // over from a prior enabled period must drain regardless (but a
    // republish still retains the maker bond).
    let slash_armed = bond_cfg
        .is_some_and(|c| c.enabled && c.slash_on_waiting_timeout && c.apply_to.applies_to_taker());
    if !slash_armed {
        release_on_timeout(pool, order.id, republishes).await;
        return Ok(None);
    }

    // Does the responsible party hold a `Locked` bond?
    let bonds = find_active_bonds_for_order(pool, order.id).await?;
    let Some(responsible) = resolve_locked_bond(order, &bonds, side).cloned() else {
        // Responsible party has no bond (e.g. the maker under
        // `apply_to = take`), or the bond already moved out of `Locked`.
        // No slash; release whatever is still active on the order
        // (retaining the maker bond on a republish).
        release_on_timeout(pool, order.id, republishes).await;
        return Ok(None);
    };

    // Settle the responsible bond's HTLC + CAS → PendingPayout(Timeout)
    // (the Phase 2 `slash_one` primitive), then resolve the remaining
    // active bonds: release them — but on a republish retain the maker's
    // still-`Locked` bond, which the abandoning taker's timeout must not
    // disturb. (We intentionally do **not** route this through
    // `apply_bond_resolution`, which always releases every non-slashed
    // bond — that is correct for a terminal dispute resolution but would
    // wrongly release the maker on a republish.)
    let node_share_pct = Settings::get_bond().map_or(0.0, |c| c.slash_node_share_pct);
    slash_one(
        pool,
        ln_client,
        &responsible,
        BondSlashReason::Timeout,
        node_share_pct,
    )
    .await;
    let maker = BondRole::Maker.to_string();
    for bond in bonds.iter() {
        if bond.id == responsible.id {
            continue;
        }
        if republishes && bond.role == maker {
            continue;
        }
        if let Err(e) = release_bond(pool, bond).await {
            warn!(
                bond_id = %bond.id,
                order_id = %order.id,
                "scheduler_timeout: release_bond failed: {}", e
            );
        }
    }

    // Confirm the slash actually landed before claiming it: a transient
    // settle failure leaves the bond `Locked` (`slash_one` is best-effort),
    // and we must never tell a user their bond was forfeited while the HTLC
    // is still theirs.
    if timeout_slash_confirmed(pool, responsible.id).await? {
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
            "timeout slash did not land (bond still Locked); no forfeiture notice sent"
        );
        Ok(None)
    }
}

/// Confirm a timeout slash actually landed on `bond_id`, regardless of
/// where the concurrent payout scheduler has since moved the row.
///
/// The slash CAS in [`slash_one`] writes `slashed_reason = Timeout`
/// atomically with the `Locked → PendingPayout` transition, and **no**
/// later transition clears it: the payout job's state changes
/// (`PendingPayout → Slashed | Forfeited | Failed`, and the
/// `Failed → PendingPayout` resurrection) only ever rewrite `state`, never
/// `slashed_reason` / `slashed_at`. So `slashed_reason = Timeout` is a
/// stable witness that *this* slash succeeded.
///
/// A point-in-time `state = PendingPayout` check would be racy: the payout
/// scheduler runs every 60s and can move a just-slashed row off
/// `PendingPayout` within the confirmation window (e.g. `finalize_node_only`
/// flips a node-only slash, `slash_node_share_pct = 1.0`, straight to
/// `Slashed`). Keying on the durable slash metadata instead means the
/// forfeiture notice is never lost to that race.
///
/// A transient settle failure leaves the bond `Locked` with
/// `slashed_reason` NULL, so this still returns `false` and no false
/// forfeiture notice is sent. Dispute slashes write
/// `slashed_reason = LostDispute`, so a (vanishingly unlikely) concurrent
/// dispute slash that won the CAS first does not trigger a *timeout*
/// notice here — the dispute path owns its own messaging.
async fn timeout_slash_confirmed(pool: &Pool<Sqlite>, bond_id: Uuid) -> Result<bool, MostroError> {
    let row: Option<(Option<String>,)> =
        sqlx::query_as("SELECT slashed_reason FROM bonds WHERE id = ?")
            .bind(bond_id)
            .fetch_optional(pool)
            .await
            .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
    let timeout = BondSlashReason::Timeout.to_string();
    Ok(row.and_then(|(reason,)| reason).as_deref() == Some(timeout.as_str()))
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

/// Phase 6 — record a proportional slash of a range maker bond against the
/// taken slice `slice`, **without settling the parent HTLC**.
///
/// A range maker posts a single hold invoice (the `parent_bond`) sized
/// against `max_amount`. A BOLT11 hold invoice is all-or-nothing, so we
/// cannot claim only one slice's share mid-range. Instead we insert a child
/// row recording the share and leave the parent `Locked`; the actual settle
/// happens once, at range close
/// ([`resolve_range_maker_bond_at_close`], Option A / "accumulate and
/// settle-at-close").
///
/// The share is **price-invariant**: `slice.fiat_amount / root.max_amount`
/// (both fiat — the ratio is independent of any BTC price drift between
/// publication and take), times the locked bond amount. The child row's
/// `order_id` and maker-side `pubkey` are the slice's, so the Phase 3
/// recipient resolver pays the slice's *other* side (the winner).
async fn record_maker_slice_slash(
    pool: &Pool<Sqlite>,
    slice: &Order,
    root: &Order,
    parent_bond: &Bond,
    reason: BondSlashReason,
    node_share_pct: f64,
) -> Result<(), MostroError> {
    let kind = slice.get_order_kind().map_err(MostroInternalErr)?;
    let maker_slice_pubkey = match kind {
        Kind::Sell => slice.seller_pubkey.as_deref(),
        Kind::Buy => slice.buyer_pubkey.as_deref(),
    };
    let Some(maker_slice_pubkey) = maker_slice_pubkey else {
        warn!(
            bond_id = %parent_bond.id,
            slice_order_id = %slice.id,
            "record_maker_slice_slash: slice has no maker-side pubkey; skipping"
        );
        return Ok(());
    };
    let Some(max_fiat) = root.max_amount.filter(|m| *m > 0) else {
        warn!(
            bond_id = %parent_bond.id,
            root_order_id = %root.id,
            "record_maker_slice_slash: range root missing positive max_amount; skipping"
        );
        return Ok(());
    };

    // Price-invariant proportional share, clamped so the cumulative slashed
    // share can never exceed the locked bond (rounding guard).
    let raw = (parent_bond.amount_sats as f64 * slice.fiat_amount as f64 / max_fiat as f64).round()
        as i64;
    let remaining = (parent_bond.amount_sats - parent_bond.slashed_share_sats).max(0);
    let slash_amount = raw.clamp(0, remaining);
    if slash_amount <= 0 {
        warn!(
            bond_id = %parent_bond.id,
            slice_order_id = %slice.id,
            raw, remaining,
            "record_maker_slice_slash: computed non-positive / over-allocated share; skipping"
        );
        return Ok(());
    }

    let now = Utc::now().timestamp();
    let node_share = compute_node_share(slash_amount, node_share_pct);

    // Insert the child slash row **atomically** with an existence check, so a
    // slice is slashed at most once under a given parent. This must be a
    // single statement, not a read-then-`create_bond`: admin settle/cancel
    // has two independent entry points (the serial Nostr loop and the RPC
    // service, each with its own LND client), and `admin_cancel` has no
    // order-status CAS, so two concurrent duplicate cancels could otherwise
    // both pass a separate existence check and both allocate against the same
    // (single) HTLC. `INSERT ... WHERE NOT EXISTS` is atomic under SQLite's
    // write lock; the loser sees `rows_affected = 0`. `order_id` is the
    // slice's and there is no `preimage`/`hash`/`payment_request` — the child
    // shares the parent HTLC. Unset columns take their schema defaults
    // (`slashed_share_sats`/`payout_attempts`/`invoice_request_attempts` = 0,
    // the rest NULL), matching `Bond::new_requested`.
    let inserted = sqlx::query(
        "INSERT INTO bonds \
            (id, order_id, parent_bond_id, child_order_id, pubkey, role, \
             amount_sats, state, slashed_reason, node_share_sats, slashed_at, created_at) \
         SELECT ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ? \
         WHERE NOT EXISTS ( \
             SELECT 1 FROM bonds WHERE parent_bond_id = ? AND child_order_id = ?)",
    )
    .bind(Uuid::new_v4())
    .bind(slice.id)
    .bind(parent_bond.id)
    .bind(slice.id)
    .bind(maker_slice_pubkey)
    .bind(BondRole::Maker.to_string())
    .bind(slash_amount)
    .bind(BondState::PendingPayout.to_string())
    .bind(reason.to_string())
    .bind(node_share)
    .bind(now)
    .bind(now)
    .bind(parent_bond.id)
    .bind(slice.id)
    .execute(pool)
    .await
    .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
    if inserted.rows_affected() == 0 {
        info!(
            bond_id = %parent_bond.id,
            slice_order_id = %slice.id,
            "record_maker_slice_slash: slice already slashed under this parent; skipping duplicate"
        );
        return Ok(());
    }

    // Recompute the parent's running total from the authoritative slice
    // child rows (self-healing: a crash between the insert above and this
    // update is repaired by the next slash or by the close recompute). Only
    // touch a still-`Locked` parent. The refund row (`child_order_id NULL`)
    // is excluded so it never inflates the slashed total.
    sqlx::query(
        "UPDATE bonds SET slashed_share_sats = \
            (SELECT COALESCE(SUM(amount_sats), 0) FROM bonds \
               WHERE parent_bond_id = ? AND child_order_id IS NOT NULL) \
         WHERE id = ? AND state = ?",
    )
    .bind(parent_bond.id)
    .bind(parent_bond.id)
    .bind(BondState::Locked.to_string())
    .execute(pool)
    .await
    .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

    info!(
        bond_id = %parent_bond.id,
        slice_order_id = %slice.id,
        reason = %reason,
        slash_amount,
        "Phase 6: recorded proportional maker slice slash (parent HTLC stays Locked)"
    );
    Ok(())
}

/// Phase 6 — resolve a maker bond when its order (range chain) terminates
/// (settle-at-close, "Option A").
///
/// Called from every terminal hook for an order *except* a successful
/// release that spawns a range remainder (the range continues then, so the
/// maker stays committed). Idempotent and best-effort:
///
/// - **Non-range maker bond** → released inline (`cancel_hold_invoice`).
///   Unifies the Phase 5 completion/cancel behaviour so hooks call one
///   function for both fixed and range makers.
/// - **Range, no slice ever slashed** → released: the maker gets the whole
///   bond back (the HTLC is cancelled, never charged).
/// - **Range, ≥1 slice slashed** → the single parent HTLC is settled
///   **once** (claiming the full bond into Mostro's wallet), the parent row
///   moves `Locked → Slashed`, and an unslashed-remainder **refund row**
///   (`child_order_id = NULL`, recipient = the maker) is created. The
///   per-slice child rows and the refund row — all `PendingPayout` — are
///   then driven by the Phase 3 payout scheduler, which skipped them while
///   the parent was `Locked`.
pub async fn resolve_range_maker_bond_at_close<L: SettleLightning + Send>(
    pool: &Pool<Sqlite>,
    ln_client: &mut L,
    order: &Order,
) -> Result<(), MostroError> {
    let Some(parent) = find_maker_bond_for_order(pool, order).await? else {
        return Ok(());
    };
    // Idempotent: act only on a still-`Locked` parent. A prior close (or a
    // Phase 5 inline release/slash) already moved it on.
    if parent.state != BondState::Locked.to_string() {
        return Ok(());
    }

    let root = find_range_root_order(pool, order.clone()).await?;
    let slice_children: Vec<Bond> = if root.max_amount.is_some() {
        find_child_slashes_for_parent(pool, parent.id)
            .await?
            .into_iter()
            .filter(|c| c.child_order_id.is_some())
            .collect()
    } else {
        // Non-range maker bond: no child rows exist; fall through to release.
        Vec::new()
    };

    let total_slashed: i64 = slice_children.iter().map(|c| c.amount_sats).sum();
    if total_slashed == 0 {
        // Nothing was ever slashed across the whole range (or non-range
        // happy path): release the bond back to the maker.
        return release_bond(pool, &parent).await;
    }

    // ≥1 slice slashed: settle the whole HTLC once. Settle BEFORE the CAS so
    // a transient failure leaves the parent retryably `Locked`.
    let Some(preimage) = parent.preimage.as_deref() else {
        warn!(
            bond_id = %parent.id,
            "range close: parent bond has no preimage; cannot settle — left Locked"
        );
        return Ok(());
    };
    if let Err(e) = ln_client.settle_hold_invoice(preimage).await {
        if is_already_settled_error(&e) {
            info!(
                bond_id = %parent.id,
                "range close: parent HTLC already settled (idempotent); proceeding"
            );
        } else {
            warn!(
                bond_id = %parent.id,
                "range close: settle_hold_invoice failed: {e} — leaving Locked for retry"
            );
            return Ok(());
        }
    }

    // Move the parent `Locked → Slashed` (settled & distributed via the
    // child rows + the refund row). The CAS ensures exactly one close wins
    // if two terminal hooks race; the loser must not create a second refund
    // row.
    let cas = sqlx::query("UPDATE bonds SET state = ? WHERE id = ? AND state = ?")
        .bind(BondState::Slashed.to_string())
        .bind(parent.id)
        .bind(BondState::Locked.to_string())
        .execute(pool)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
    if cas.rows_affected() != 1 {
        info!(
            bond_id = %parent.id,
            "range close: parent already closed concurrently; skipping refund row"
        );
        return Ok(());
    }

    let now = Utc::now().timestamp();

    // Phase 6: anchor each slice child's claim window at *close* time, not at
    // slice-slash time. A child can only be paid out once the parent HTLC is
    // settled (now); leaving its `slashed_at` at slice-slash time would let
    // the `payout_claim_window_days` countdown run while the bond was still
    // unpayable, so a range that stayed open past the window could forfeit a
    // child the instant it became processable, before the counterparty was
    // ever asked for an invoice. (In the dispute path close follows the slash
    // almost immediately, but the timeout-slash path in Phase 7 may not.)
    if !slice_children.is_empty() {
        sqlx::query(
            "UPDATE bonds SET slashed_at = ? \
             WHERE parent_bond_id = ? AND child_order_id IS NOT NULL AND state = ?",
        )
        .bind(now)
        .bind(parent.id)
        .bind(BondState::PendingPayout.to_string())
        .execute(pool)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
    }

    let refund_amount = (parent.amount_sats - total_slashed).max(0);
    info!(
        bond_id = %parent.id,
        order_id = %root.id,
        amount_sats = parent.amount_sats,
        total_slashed,
        refund_amount,
        children = slice_children.len(),
        "Phase 6: range maker bond settled at close; distributing child shares + maker refund"
    );

    if refund_amount > 0 {
        // Inherit a parseable reason from a slice child so the Phase 3
        // `slashed_reason` invariant holds; the refund recipient is resolved
        // directly from the row (the maker), not via the reason.
        let reason = slice_children
            .first()
            .and_then(|c| c.slashed_reason.clone())
            .unwrap_or_else(|| BondSlashReason::LostDispute.to_string());
        let mut refund = Bond::new_requested(
            root.id,
            parent.pubkey.clone(),
            BondRole::Maker,
            refund_amount,
        );
        refund.parent_bond_id = Some(parent.id);
        refund.child_order_id = None; // marks the maker-refund row
        refund.state = BondState::PendingPayout.to_string();
        refund.slashed_reason = Some(reason);
        refund.slashed_at = Some(now);
        refund.node_share_sats = Some(0); // full refund to the maker
        create_bond(pool, refund).await?;
    }

    Ok(())
}

/// Open an `LndConnector` and run [`resolve_range_maker_bond_at_close`],
/// logging on failure. For the non-admin terminal hooks (completion,
/// cancel, scheduler expiry) that don't already hold an LND client. A cheap
/// pre-check skips opening LND entirely when there is no `Locked` maker bond
/// to resolve — the common case. The admin handlers pass their own
/// `ln_client` to the generic function directly.
pub async fn resolve_range_maker_bond_at_close_or_warn(
    pool: &Pool<Sqlite>,
    order: &Order,
    context: &'static str,
) {
    let locked = BondState::Locked.to_string();
    match find_maker_bond_for_order(pool, order).await {
        Ok(Some(b)) if b.state == locked => {}
        Ok(_) => return,
        Err(e) => {
            warn!("{context}: maker bond lookup failed for {}: {e}", order.id);
            return;
        }
    }
    let mut ln = match LndConnector::new().await {
        Ok(l) => l,
        Err(e) => {
            warn!("{context}: cannot connect to LND to resolve maker bond at close: {e}");
            return;
        }
    };
    if let Err(e) = resolve_range_maker_bond_at_close(pool, &mut ln, order).await {
        warn!(
            order_id = %order.id,
            "{context}: resolve_range_maker_bond_at_close failed: {e}"
        );
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
        // Phase 6 chain-walk tests load full `Order` rows via `Order::by_id`,
        // which selects every column the model declares — so the orders
        // table must carry the later Cashu columns too.
        for stmt in include_str!("../../../migrations/20260530120000_cashu_escrow_fields.sql")
            .split(';')
            .map(str::trim)
            .filter(|s| !s.is_empty() && !s.lines().all(|l| l.trim_start().starts_with("--")))
        {
            sqlx::query(stmt)
                .execute(&pool)
                .await
                .expect("cashu_escrow_fields migration");
        }
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
        insert_bond_with_role(pool, order_id, pubkey, BondRole::Taker, state).await
    }

    /// Phase 5: same fixture as [`insert_bond`] but parameterised on the
    /// posting role, so maker-bond dispute-slash tests can assert the
    /// buyer/seller → bond-row resolution resolves to the maker row when
    /// the maker is on the named side (§3.1).
    async fn insert_bond_with_role(
        pool: &Pool<Sqlite>,
        order_id: Uuid,
        pubkey: &str,
        role: BondRole,
        state: BondState,
    ) -> Bond {
        let mut b = Bond::new_requested(order_id, pubkey.to_string(), role, 10_000);
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
    async fn apply_slash_seller_on_sell_order_transitions_maker_bond() {
        // Phase 5 (§10.2 / §10.4 acceptance bullet 3): on a sell-order
        // the maker IS the seller, so `slash_seller=true` must resolve to
        // the maker's bond row via the §3.1 pubkey mapping — even though
        // the resolver is role-agnostic and matches on
        // `order.seller_pubkey`. With both a maker bond (seller) and a
        // taker bond (buyer) posted, slashing only the seller transitions
        // the maker bond to PendingPayout and releases the taker bond,
        // proving the two sides resolve orthogonally.
        let pool = setup_pool().await;
        let order = fixture_order(Kind::Sell, maker_pk(), taker_pk());
        insert_order_row(&pool, &order).await;
        let maker_bond = insert_bond_with_role(
            &pool,
            order.id,
            maker_pk(),
            BondRole::Maker,
            BondState::Locked,
        )
        .await;
        let taker_bond = insert_bond(&pool, order.id, taker_pk(), BondState::Locked).await;
        let res = BondResolution {
            slash_seller: true,
            slash_buyer: false,
        };

        // Pre-flight validation must accept the directive: the seller
        // (maker) holds a Locked bond.
        validate_bond_resolution(&pool, &order, &res).await.unwrap();

        apply_bond_resolution(
            &pool,
            &mut StubSettle::new(),
            &order,
            &res,
            BondSlashReason::LostDispute,
        )
        .await
        .unwrap();

        let maker_row: (String, Option<String>) =
            sqlx::query_as("SELECT state, slashed_reason FROM bonds WHERE id = ?")
                .bind(maker_bond.id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            maker_row.0,
            BondState::PendingPayout.to_string(),
            "maker bond must be slashed on slash_seller for a sell order"
        );
        assert_eq!(maker_row.1.as_deref(), Some("lost-dispute"));

        let taker_state: String = sqlx::query_scalar("SELECT state FROM bonds WHERE id = ?")
            .bind(taker_bond.id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            taker_state,
            BondState::Released.to_string(),
            "the non-slashed taker bond must be released, not settled"
        );
    }

    #[tokio::test]
    async fn apply_slash_buyer_on_buy_order_transitions_maker_bond() {
        // Phase 5 (§3.1 mirror): on a buy-order the maker IS the buyer,
        // so `slash_buyer=true` resolves to the maker's bond row. This is
        // the buy-order counterpart of the sell-order test above and
        // completes the §10.4 acceptance bullet 3 matrix.
        let pool = setup_pool().await;
        // Buy order: maker is the buyer, taker is the seller.
        let order = fixture_order(Kind::Buy, taker_pk(), maker_pk());
        insert_order_row(&pool, &order).await;
        let maker_bond = insert_bond_with_role(
            &pool,
            order.id,
            maker_pk(),
            BondRole::Maker,
            BondState::Locked,
        )
        .await;
        let res = BondResolution {
            slash_seller: false,
            slash_buyer: true,
        };

        validate_bond_resolution(&pool, &order, &res).await.unwrap();
        apply_bond_resolution(
            &pool,
            &mut StubSettle::new(),
            &order,
            &res,
            BondSlashReason::LostDispute,
        )
        .await
        .unwrap();

        let row: (String, Option<String>) =
            sqlx::query_as("SELECT state, slashed_reason FROM bonds WHERE id = ?")
                .bind(maker_bond.id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            row.0,
            BondState::PendingPayout.to_string(),
            "maker bond must be slashed on slash_buyer for a buy order"
        );
        assert_eq!(row.1.as_deref(), Some("lost-dispute"));
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
    async fn timeout_republish_retains_maker_bond_sell_order() {
        // Regression (PR #767 review): sell order, WaitingBuyerInvoice. The
        // buyer (taker) times out, so the order is **republished** to the
        // book — the maker (seller) is still committed to it. The taker
        // bond must be slashed, but the maker bond must stay `Locked`:
        // releasing it would put a takeable order back in the book with no
        // maker bond backing it. It is only released when the order itself
        // terminates.
        let pool = setup_pool().await;
        let order = waiting_order(
            Kind::Sell,
            maker_pk(),
            taker_pk(),
            Status::WaitingBuyerInvoice,
        );
        insert_order_row(&pool, &order).await;
        let maker_bond = insert_bond_with_role(
            &pool,
            order.id,
            maker_pk(),
            BondRole::Maker,
            BondState::Locked,
        )
        .await;
        let taker_bond = insert_bond(&pool, order.id, taker_pk(), BondState::Locked).await;
        let mut ln = StubSettle::new();

        // apply_to=both so a maker bond is in play alongside the taker bond.
        let cfg = timeout_cfg(true, true, BondApplyTo::Both);
        let result = slash_or_release_on_timeout(&pool, &mut ln, &order, Some(&cfg))
            .await
            .unwrap();

        assert_eq!(
            result.map(|b| b.id),
            Some(taker_bond.id),
            "the abandoning taker's bond is the one slashed"
        );
        assert_eq!(
            read_bond_state(&pool, taker_bond.id).await,
            BondState::PendingPayout.to_string(),
            "taker bond is slashed on the republish path"
        );
        assert_eq!(
            read_bond_state(&pool, maker_bond.id).await,
            BondState::Locked.to_string(),
            "maker bond must stay Locked when the order is republished"
        );
        assert_eq!(
            ln.calls(),
            vec![stub_preimage()],
            "only the slashed taker HTLC is settled; the maker HTLC is untouched"
        );
    }

    #[tokio::test]
    async fn timeout_republish_with_no_slash_still_retains_maker_bond() {
        // Same republish scenario but with the slash gate closed
        // (slash_on_waiting_timeout = false). The taker bond drains via the
        // Phase 1 release, but the maker bond must still be retained — the
        // order goes back to the book with the maker committed.
        let pool = setup_pool().await;
        let order = waiting_order(
            Kind::Sell,
            maker_pk(),
            taker_pk(),
            Status::WaitingBuyerInvoice,
        );
        insert_order_row(&pool, &order).await;
        let maker_bond = insert_bond_with_role(
            &pool,
            order.id,
            maker_pk(),
            BondRole::Maker,
            BondState::Locked,
        )
        .await;
        let taker_bond = insert_bond(&pool, order.id, taker_pk(), BondState::Locked).await;
        let mut ln = StubSettle::new();

        let cfg = timeout_cfg(true, false, BondApplyTo::Both);
        let result = slash_or_release_on_timeout(&pool, &mut ln, &order, Some(&cfg))
            .await
            .unwrap();

        assert!(result.is_none(), "gate closed → no slash reported");
        assert!(ln.calls().is_empty(), "release path never settles an HTLC");
        assert_eq!(
            read_bond_state(&pool, taker_bond.id).await,
            BondState::Released.to_string(),
            "taker bond is released when the slash gate is closed"
        );
        assert_eq!(
            read_bond_state(&pool, maker_bond.id).await,
            BondState::Locked.to_string(),
            "maker bond is retained on republish even when no slash happens"
        );
    }

    #[tokio::test]
    async fn timeout_cancel_releases_maker_bond_sell_order() {
        // Counterpart to the republish case: sell order, WaitingPayment.
        // The seller (maker) is responsible, so the order is **cancelled**
        // outright (not republished). Because the order terminates, the
        // maker bond must be released — the retain-on-republish carve-out
        // must NOT leak into the terminal cancel path. Gate closed
        // (slash_on_waiting_timeout = false) keeps this purely about the
        // release routing, mirroring the republish/no-slash test above.
        let pool = setup_pool().await;
        let order = waiting_order(Kind::Sell, maker_pk(), taker_pk(), Status::WaitingPayment);
        insert_order_row(&pool, &order).await;
        let maker_bond = insert_bond_with_role(
            &pool,
            order.id,
            maker_pk(),
            BondRole::Maker,
            BondState::Locked,
        )
        .await;
        let taker_bond = insert_bond(&pool, order.id, taker_pk(), BondState::Locked).await;
        let mut ln = StubSettle::new();

        let cfg = timeout_cfg(true, false, BondApplyTo::Both);
        let result = slash_or_release_on_timeout(&pool, &mut ln, &order, Some(&cfg))
            .await
            .unwrap();

        assert!(result.is_none());
        assert!(ln.calls().is_empty());
        assert_eq!(
            read_bond_state(&pool, maker_bond.id).await,
            BondState::Released.to_string(),
            "maker bond is released when the order is cancelled (terminal)"
        );
        assert_eq!(
            read_bond_state(&pool, taker_bond.id).await,
            BondState::Released.to_string(),
            "taker bond is released too on a terminal cancel"
        );
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
    async fn timeout_slash_confirmed_survives_concurrent_payout_progression() {
        // Regression: the payout scheduler runs concurrently (every 60s)
        // and can move a just-slashed `PendingPayout` row onward — e.g. a
        // node-only slash flips straight to `Slashed` via
        // `finalize_node_only` — before the dispatch re-reads it. The
        // confirmation must key off the durable `slashed_reason = Timeout`
        // metadata, not the transient `PendingPayout` state, so the
        // forfeiture notice is never lost to that race. Every post-slash
        // state must confirm.
        let pool = setup_pool().await;
        let order = waiting_order(
            Kind::Sell,
            maker_pk(),
            taker_pk(),
            Status::WaitingBuyerInvoice,
        );
        insert_order_row(&pool, &order).await;

        for state in [
            BondState::PendingPayout,
            BondState::Slashed,
            BondState::Forfeited,
            BondState::Failed,
        ] {
            let bond = insert_bond(&pool, order.id, taker_pk(), state).await;
            // Stamp the timeout slash metadata that `slash_one` writes
            // atomically with the Locked → PendingPayout CAS.
            sqlx::query("UPDATE bonds SET slashed_reason = ? WHERE id = ?")
                .bind(BondSlashReason::Timeout.to_string())
                .bind(bond.id)
                .execute(&pool)
                .await
                .unwrap();
            assert!(
                timeout_slash_confirmed(&pool, bond.id).await.unwrap(),
                "post-slash state {state:?} with slashed_reason=Timeout must confirm the slash"
            );
        }

        // A still-Locked bond (transient settle failure) carries no slash
        // metadata and must NOT confirm — no false forfeiture notice.
        let locked = insert_bond(&pool, order.id, taker_pk(), BondState::Locked).await;
        assert!(
            !timeout_slash_confirmed(&pool, locked.id).await.unwrap(),
            "a Locked bond with no slash metadata must not confirm"
        );

        // A dispute slash (LostDispute) must not be mistaken for a timeout
        // slash, so the timeout notice is not sent for it.
        let dispute = insert_bond(&pool, order.id, taker_pk(), BondState::PendingPayout).await;
        sqlx::query("UPDATE bonds SET slashed_reason = ? WHERE id = ?")
            .bind(BondSlashReason::LostDispute.to_string())
            .bind(dispute.id)
            .execute(&pool)
            .await
            .unwrap();
        assert!(
            !timeout_slash_confirmed(&pool, dispute.id).await.unwrap(),
            "a LostDispute slash must not confirm as a timeout slash"
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

    // ── Phase 6: range-order maker bond ────────────────────────────────

    use crate::app::bond::db::{
        find_bond_by_id, find_child_slashes_for_parent, find_maker_bond_for_order,
        find_range_root_order,
    };

    /// Insert an order row including the range columns (`min_amount`,
    /// `max_amount`, `range_parent_id`) that `insert_order_row` omits.
    async fn insert_range_order_row(pool: &Pool<Sqlite>, order: &Order) {
        sqlx::query(
            r#"INSERT INTO orders (
                id, kind, event_id, status, premium, payment_method,
                amount, fiat_code, fiat_amount, min_amount, max_amount,
                range_parent_id, created_at, expires_at, seller_pubkey, buyer_pubkey
            ) VALUES (?, ?, ?, ?, 0, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
        )
        .bind(order.id)
        .bind(&order.kind)
        .bind(order.id.simple().to_string())
        .bind(&order.status)
        .bind(&order.payment_method)
        .bind(order.amount)
        .bind(&order.fiat_code)
        .bind(order.fiat_amount)
        .bind(order.min_amount)
        .bind(order.max_amount)
        .bind(order.range_parent_id)
        .bind(order.created_at)
        .bind(order.expires_at)
        .bind(order.seller_pubkey.as_deref())
        .bind(order.buyer_pubkey.as_deref())
        .execute(pool)
        .await
        .expect("insert range order");
    }

    /// A range-order slice: `amount = 0`, `min`/`max` set (so
    /// `is_range_order()` and the root-`max_amount` check hold).
    fn range_slice(
        kind: Kind,
        seller_pk: &str,
        buyer_pk: &str,
        fiat_amount: i64,
        min: i64,
        max: i64,
    ) -> Order {
        let mut o = fixture_order(kind, seller_pk, buyer_pk);
        o.amount = 0;
        o.fiat_amount = fiat_amount;
        o.min_amount = Some(min);
        o.max_amount = Some(max);
        o
    }

    /// A `Locked` parent maker bond with a settleable preimage and **no**
    /// `hash` (so the release path skips the live LND `cancel_hold_invoice`
    /// and the settle path is exercised only via the `StubSettle`).
    async fn insert_parent_maker_bond(
        pool: &Pool<Sqlite>,
        order_id: Uuid,
        pubkey: &str,
        amount_sats: i64,
    ) -> Bond {
        let mut b = Bond::new_requested(order_id, pubkey.to_string(), BondRole::Maker, amount_sats);
        b.state = BondState::Locked.to_string();
        b.preimage = Some(stub_preimage());
        b.hash = None;
        sqlx_crud::Crud::create(b.clone(), pool).await.unwrap();
        b
    }

    #[tokio::test]
    async fn record_maker_slice_slash_is_proportional() {
        // sell range order: maker = seller. max fiat = 100, slice fiat = 40,
        // bond = 1000 → slash = round(1000 * 40/100) = 400; node 50% = 200.
        let pool = setup_pool().await;
        let root = range_slice(Kind::Sell, maker_pk(), taker_pk(), 40, 10, 100);
        insert_range_order_row(&pool, &root).await;
        let parent = insert_parent_maker_bond(&pool, root.id, maker_pk(), 1000).await;

        record_maker_slice_slash(
            &pool,
            &root,
            &root,
            &parent,
            BondSlashReason::LostDispute,
            0.5,
        )
        .await
        .unwrap();

        let children = find_child_slashes_for_parent(&pool, parent.id)
            .await
            .unwrap();
        assert_eq!(children.len(), 1);
        let c = &children[0];
        assert_eq!(c.amount_sats, 400, "proportional slice share");
        assert_eq!(c.node_share_sats, Some(200));
        assert_eq!(c.state, BondState::PendingPayout.to_string());
        assert_eq!(c.parent_bond_id, Some(parent.id));
        assert_eq!(c.child_order_id, Some(root.id));
        assert_eq!(c.order_id, root.id);
        // Maker's slice-side (seller) key, so Phase 3 pays the buyer winner.
        assert_eq!(c.pubkey, maker_pk());
        assert!(c.slashed_at.is_some());
        assert!(c.preimage.is_none(), "child shares the parent HTLC");

        // Parent accumulates the running total but stays Locked (no settle).
        let p = find_bond_by_id(&pool, parent.id).await.unwrap().unwrap();
        assert_eq!(p.slashed_share_sats, 400);
        assert_eq!(p.state, BondState::Locked.to_string());
    }

    #[tokio::test]
    async fn record_maker_slice_slash_clamps_cumulative_to_bond() {
        // Two slices that together exceed the bond must clamp so the
        // cumulative slashed share never exceeds `amount_sats`.
        let pool = setup_pool().await;
        let root = range_slice(Kind::Sell, maker_pk(), taker_pk(), 80, 10, 100);
        insert_range_order_row(&pool, &root).await;
        let parent = insert_parent_maker_bond(&pool, root.id, maker_pk(), 1000).await;

        // First slice 80/100 → 800.
        record_maker_slice_slash(
            &pool,
            &root,
            &root,
            &parent,
            BondSlashReason::LostDispute,
            0.0,
        )
        .await
        .unwrap();
        // Reload parent (slashed_share_sats now 800) and slash a *second*,
        // distinct slice (its own order row) of another 80/100 → raw 800, but
        // only 200 remaining → clamp to 200.
        let parent = find_bond_by_id(&pool, parent.id).await.unwrap().unwrap();
        let slice2 = range_slice(Kind::Sell, maker_pk(), taker_pk(), 80, 10, 100);
        insert_range_order_row(&pool, &slice2).await;
        record_maker_slice_slash(
            &pool,
            &slice2,
            &root,
            &parent,
            BondSlashReason::LostDispute,
            0.0,
        )
        .await
        .unwrap();

        let children = find_child_slashes_for_parent(&pool, parent.id)
            .await
            .unwrap();
        let total: i64 = children.iter().map(|c| c.amount_sats).sum();
        assert_eq!(total, 1000, "cumulative slash clamped to the bond amount");
    }

    #[tokio::test]
    async fn range_close_no_slashes_releases_maker_bond() {
        let pool = setup_pool().await;
        let root = range_slice(Kind::Sell, maker_pk(), taker_pk(), 40, 10, 100);
        insert_range_order_row(&pool, &root).await;
        let parent = insert_parent_maker_bond(&pool, root.id, maker_pk(), 1000).await;

        let stub = StubSettle::new();
        resolve_range_maker_bond_at_close(&pool, &mut stub.clone(), &root)
            .await
            .unwrap();

        assert!(
            stub.calls().is_empty(),
            "no slice was ever slashed → the HTLC must be cancelled, not settled"
        );
        let p = find_bond_by_id(&pool, parent.id).await.unwrap().unwrap();
        assert_eq!(p.state, BondState::Released.to_string());
        assert!(find_child_slashes_for_parent(&pool, parent.id)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn range_close_with_one_slash_settles_once_and_refunds_remainder() {
        let pool = setup_pool().await;
        let root = range_slice(Kind::Sell, maker_pk(), taker_pk(), 40, 10, 100);
        insert_range_order_row(&pool, &root).await;
        let parent = insert_parent_maker_bond(&pool, root.id, maker_pk(), 1000).await;
        record_maker_slice_slash(
            &pool,
            &root,
            &root,
            &parent,
            BondSlashReason::LostDispute,
            0.5,
        )
        .await
        .unwrap();

        let stub = StubSettle::new();
        resolve_range_maker_bond_at_close(&pool, &mut stub.clone(), &root)
            .await
            .unwrap();

        // Settled exactly once, with the parent preimage.
        assert_eq!(stub.calls(), vec![stub_preimage()]);
        let p = find_bond_by_id(&pool, parent.id).await.unwrap().unwrap();
        assert_eq!(p.state, BondState::Slashed.to_string());

        let children = find_child_slashes_for_parent(&pool, parent.id)
            .await
            .unwrap();
        assert_eq!(children.len(), 2, "slice slash + maker refund row");
        let refund = children
            .iter()
            .find(|c| c.child_order_id.is_none())
            .expect("a maker-refund row");
        assert_eq!(refund.amount_sats, 600, "1000 bond - 400 slashed");
        assert_eq!(refund.node_share_sats, Some(0), "full refund to the maker");
        assert_eq!(refund.pubkey, maker_pk());
        assert_eq!(refund.order_id, root.id);
        assert_eq!(refund.state, BondState::PendingPayout.to_string());

        // Idempotent: a second close (parent now Slashed) is a no-op.
        resolve_range_maker_bond_at_close(&pool, &mut stub.clone(), &root)
            .await
            .unwrap();
        assert_eq!(stub.calls().len(), 1, "no second settle");
        assert_eq!(
            find_child_slashes_for_parent(&pool, parent.id)
                .await
                .unwrap()
                .len(),
            2,
            "no duplicate refund row"
        );
    }

    #[tokio::test]
    async fn apply_range_maker_slash_records_child_without_settling() {
        let pool = setup_pool().await;
        // sell range order: `slash_seller` targets the maker (seller).
        let root = range_slice(Kind::Sell, maker_pk(), taker_pk(), 40, 10, 100);
        insert_range_order_row(&pool, &root).await;
        let parent = insert_parent_maker_bond(&pool, root.id, maker_pk(), 1000).await;

        let res = BondResolution {
            slash_seller: true,
            slash_buyer: false,
        };
        let stub = StubSettle::new();
        apply_bond_resolution(
            &pool,
            &mut stub.clone(),
            &root,
            &res,
            BondSlashReason::LostDispute,
        )
        .await
        .unwrap();

        assert!(
            stub.calls().is_empty(),
            "a range maker slash records a child row but must NOT settle inline"
        );
        let p = find_bond_by_id(&pool, parent.id).await.unwrap().unwrap();
        assert_eq!(p.state, BondState::Locked.to_string());
        let children = find_child_slashes_for_parent(&pool, parent.id)
            .await
            .unwrap();
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].amount_sats, 400);
        assert_eq!(children[0].child_order_id, Some(root.id));
    }

    #[tokio::test]
    async fn maker_bond_resolves_from_descendant_slice() {
        // The maker bond lives on the range root; a slash on a descendant
        // slice must still find it by walking `range_parent_id`.
        let pool = setup_pool().await;
        let root = range_slice(Kind::Sell, maker_pk(), taker_pk(), 30, 10, 100);
        insert_range_order_row(&pool, &root).await;
        let parent = insert_parent_maker_bond(&pool, root.id, maker_pk(), 1000).await;

        let mut c1 = range_slice(Kind::Sell, maker_pk(), taker_pk(), 40, 10, 70);
        c1.range_parent_id = Some(root.id);
        insert_range_order_row(&pool, &c1).await;

        let found = find_maker_bond_for_order(&pool, &c1)
            .await
            .unwrap()
            .expect("maker bond resolved via the range root");
        assert_eq!(found.id, parent.id);

        let resolved_root = find_range_root_order(&pool, c1.clone()).await.unwrap();
        assert_eq!(resolved_root.id, root.id);
    }

    #[tokio::test]
    async fn range_close_reanchors_slice_child_claim_window() {
        // A child's payout claim window must start at *close* time, not at
        // slice-slash time — otherwise a long-open range could forfeit the
        // child the instant it becomes payable.
        let pool = setup_pool().await;
        let root = range_slice(Kind::Sell, maker_pk(), taker_pk(), 40, 10, 100);
        insert_range_order_row(&pool, &root).await;
        let parent = insert_parent_maker_bond(&pool, root.id, maker_pk(), 1000).await;
        record_maker_slice_slash(
            &pool,
            &root,
            &root,
            &parent,
            BondSlashReason::LostDispute,
            0.5,
        )
        .await
        .unwrap();

        // Backdate the slice child's slashed_at to simulate a range that
        // stayed open well past the claim window before closing.
        sqlx::query(
            "UPDATE bonds SET slashed_at = ? WHERE parent_bond_id = ? AND child_order_id IS NOT NULL",
        )
        .bind(1_000_000i64)
        .bind(parent.id)
        .execute(&pool)
        .await
        .unwrap();

        let before_close = Utc::now().timestamp();
        resolve_range_maker_bond_at_close(&pool, &mut StubSettle::new(), &root)
            .await
            .unwrap();

        let children = find_child_slashes_for_parent(&pool, parent.id)
            .await
            .unwrap();
        let slice = children
            .iter()
            .find(|c| c.child_order_id.is_some())
            .expect("slice child");
        assert!(
            slice.slashed_at.unwrap() >= before_close,
            "slice child claim window must re-anchor at close time, got {:?}",
            slice.slashed_at
        );
    }

    #[tokio::test]
    async fn record_maker_slice_slash_is_idempotent_per_slice() {
        // Recording the same slice twice (e.g. a retry while the parent HTLC
        // is still Locked) must NOT insert a second child row or double the
        // accumulated share.
        let pool = setup_pool().await;
        let root = range_slice(Kind::Sell, maker_pk(), taker_pk(), 40, 10, 100);
        insert_range_order_row(&pool, &root).await;
        let parent = insert_parent_maker_bond(&pool, root.id, maker_pk(), 1000).await;

        record_maker_slice_slash(
            &pool,
            &root,
            &root,
            &parent,
            BondSlashReason::LostDispute,
            0.5,
        )
        .await
        .unwrap();
        // Reload parent (slashed_share_sats now 400) and replay the slash.
        let parent = find_bond_by_id(&pool, parent.id).await.unwrap().unwrap();
        record_maker_slice_slash(
            &pool,
            &root,
            &root,
            &parent,
            BondSlashReason::LostDispute,
            0.5,
        )
        .await
        .unwrap();

        let children = find_child_slashes_for_parent(&pool, parent.id)
            .await
            .unwrap();
        assert_eq!(children.len(), 1, "the slice must be slashed exactly once");
        assert_eq!(children[0].amount_sats, 400);
        let p = find_bond_by_id(&pool, parent.id).await.unwrap().unwrap();
        assert_eq!(p.slashed_share_sats, 400, "share must not double on replay");
    }
}
