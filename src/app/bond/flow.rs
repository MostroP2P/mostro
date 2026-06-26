//! Bond lifecycle wiring (Phase 1 + Phase 1.5).
//!
//! Phase 1 adds a single guarantee: when the feature is enabled and the
//! taker side is in scope (`apply_to ∈ {take, both}`), a taker is asked to
//! lock a Lightning hold invoice as a bond before the trade flow starts;
//! and on **every** exit — happy path, unilateral cancel, cooperative
//! cancel, admin action, scheduler timeout — the bond is **released**.
//!
//! Slashing is intentionally absent: it lands in Phase 2+. This means
//! operators can flip `enabled = true` in staging and exercise hold-invoice
//! custody end-to-end without any user funds at risk if Mostro mis-judges
//! the situation.
//!
//! Concurrent taker bonds (see `docs/ANTI_ABUSE_BOND.md`). Multiple
//! `Requested` taker bonds may coexist on a single order. Each take
//! creates a new bond row alongside any prior `Requested` rows; the
//! take handler does **not** mutate the order's taker fields
//! (`buyer_pubkey` / `seller_pubkey`, identities, per-take pricing,
//! buyer_invoice). That deferred context lives in the bond row's
//! `taker_*` columns until one bond wins the lock race —
//! [`on_bond_invoice_accepted`] promotes the winner's columns onto the
//! order, cancels every other still-`Requested` bond on the order,
//! and messages each loser `Action::Canceled`. A malicious taker who never
//! pays their bond does not block the order book: their HTLC expires
//! on its own LND-side TTL and the bond is released.
//!
//! Phase 1.5 protocol layer. Mostro now emits the bond bolt11 as a
//! dedicated [`Action::PayBondInvoice`] (not the generic
//! [`Action::PayInvoice`] reused in Phase 1) and parks the order at
//! [`Status::WaitingTakerBond`] while the bond is outstanding. The
//! wire-published NIP-33 status still maps to NIP-69 `pending`
//! (`nip33::create_status_tags`) so the order keeps advertising as
//! available — `WaitingTakerBond` is purely a daemon-internal
//! distinction between "matched, awaiting bond" and "advertised, no
//! taker yet". On `Locked` the status transitions out to
//! `WaitingPayment` / `WaitingBuyerInvoice` via `resume_take_after_bond`;
//! on bond release before lock (taker abandons, taker self-cancel, or
//! losing the lock race), if no other active bond remains on the
//! order, the status flips back to `Pending`.

use std::str::FromStr;
use std::sync::Arc;

use chrono::Utc;
use fedimint_tonic_lnd::lnrpc::invoice::InvoiceState;
use mostro_core::db::Crud;
use mostro_core::error::{MostroError, MostroError::MostroInternalErr, ServiceError};
use mostro_core::prelude::*;
use nostr_sdk::nostr::hashes::hex::FromHex;
use nostr_sdk::prelude::*;
use sqlx::{Pool, Sqlite};
use tokio::sync::mpsc::channel;
use tracing::{info, warn};
use uuid::Uuid;

use crate::config::settings::Settings;
use crate::lightning::{InvoiceMessage, LndConnector};
use crate::util::{
    bytes_to_string, enqueue_order_msg, get_keys, set_waiting_invoice_status, show_hold_invoice,
};

use super::db::{create_bond, find_active_bonds, find_active_bonds_for_order, find_bond_by_hash};
use super::math::compute_bond_amount;
use super::model::Bond;
use super::types::{BondRole, BondState};

/// True when the configuration requires the **taker** to post a bond.
///
/// This is the single Phase 1 gate. Every bond touchpoint in the take
/// flow asks this question first, so a misconfigured node (no
/// `[anti_abuse_bond]` block at all) behaves exactly like before.
pub fn taker_bond_required() -> bool {
    Settings::get_bond()
        .filter(|cfg| cfg.enabled)
        .is_some_and(|cfg| cfg.apply_to.applies_to_taker())
}

/// True when the configuration requires the **maker** to post a bond.
///
/// Phase 5/6 gate, symmetric to [`taker_bond_required`]. `publish_order`
/// asks this question before publishing a new order to Nostr: when it is
/// true the order is parked at [`Status::WaitingMakerBond`] and no NIP-33
/// event is emitted until the maker locks the bond. Both fixed-amount
/// (Phase 5) and range (Phase 6) makers take this path; range makers size
/// the bond against `max_amount` and slash proportionally per slice.
pub fn maker_bond_required() -> bool {
    Settings::get_bond()
        .filter(|cfg| cfg.enabled)
        .is_some_and(|cfg| cfg.apply_to.applies_to_maker())
}

/// True when a `Locked` **taker** bond already exists among `bonds` — the
/// signal that the order's trade is committed and no further take may
/// begin (it must be rejected with `PendingOrderExists`).
///
/// The role scoping is load-bearing for Phase 5. Under `apply_to = both`
/// the maker's own bond is `Locked` on *every* published order — that is
/// the steady state, not a committed trade. Counting it here would reject
/// every taker. Only a `Locked` *taker* bond marks the
/// first-to-lock-wins race as decided (§6.5), so the take handlers
/// (`take_buy_action` / `take_sell_action`) gate on this predicate
/// instead of "any Locked bond".
pub fn trade_committed_by_locked_taker_bond(bonds: &[Bond]) -> bool {
    let locked = BondState::Locked.to_string();
    let taker = BondRole::Taker.to_string();
    bonds.iter().any(|b| b.state == locked && b.role == taker)
}

/// Per-take context that the take handler computed locally and now
/// stashes on the bond row instead of mutating the order.
///
/// Under concurrent taker bonds (see `docs/ANTI_ABUSE_BOND.md`), N
/// prospective takers may have outstanding `Requested` bonds on the
/// same order. The order row therefore can't carry "this take's"
/// pubkey / invoice / per-take pricing until exactly one bond wins
/// the lock race — `on_bond_invoice_accepted` copies the winning
/// bond's `taker_*` columns onto the order at that point.
#[derive(Debug, Clone)]
pub struct TakerContext {
    /// Identity (master) pubkey of the taker.
    pub identity: String,
    /// Trade index from the take message.
    pub trade_index: i64,
    /// Buyer payout invoice supplied by the taker (sell-order takes
    /// only; `None` for buy-order takes).
    pub buyer_invoice: Option<String>,
    /// Fiat amount this take committed to (matters for range orders).
    pub fiat_amount: i64,
    /// Sats amount this take committed to. For market-priced range
    /// orders this is the per-bond quote snapshot.
    pub amount: i64,
    /// Mostro fee snapshot for this take.
    pub fee: i64,
    /// Dev fee snapshot for this take.
    pub dev_fee: i64,
}

/// Create a hold invoice for the taker's bond, persist a `Bond` row in
/// `Requested`, ship the bolt11 to the taker, and start the LND
/// subscriber that flips the row to `Locked` once the taker pays.
///
/// On any failure inside this function the bond row may exist in
/// `Requested` with no LND counterpart — that's fine: Phase 1's
/// "always release" guarantee covers it on the next exit.
///
/// Under concurrent taker bonds, `taker_ctx` carries the per-take
/// context (taker pubkey is already on the bond row; identity, trade
/// index, per-bond pricing, etc. live in `taker_*` columns) so the
/// order row stays unmutated until the winning bond locks.
pub async fn request_taker_bond(
    pool: &Pool<Sqlite>,
    order: &Order,
    taker_pubkey: PublicKey,
    request_id: Option<u64>,
    taker_ctx: TakerContext,
) -> Result<Bond, MostroError> {
    let cfg = Settings::get_bond().ok_or_else(|| {
        MostroInternalErr(ServiceError::UnexpectedError(
            "anti_abuse_bond block is missing while bond was deemed required".into(),
        ))
    })?;

    // Bond amount is derived from the per-take sats amount (not
    // `order.amount`), so concurrent takers on a market-priced range
    // order each post a bond sized to their own quote.
    let amount = compute_bond_amount(taker_ctx.amount, cfg);
    let memo = format!("mostro bond order_id={}", order.id);

    let mut ln_client = LndConnector::new().await?;
    let (invoice_resp, preimage, hash) = ln_client
        .create_hold_invoice(&memo, amount)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::HoldInvoiceError(e.to_string())))?;

    let mut bond = Bond::new_requested(order.id, taker_pubkey.to_string(), BondRole::Taker, amount);
    bond.hash = Some(bytes_to_string(&hash));
    bond.preimage = Some(bytes_to_string(&preimage));
    bond.payment_request = Some(invoice_resp.payment_request.clone());
    bond.taker_identity = Some(taker_ctx.identity.clone());
    bond.taker_trade_index = Some(taker_ctx.trade_index);
    bond.taker_invoice = taker_ctx.buyer_invoice.clone();
    bond.taker_fiat_amount = Some(taker_ctx.fiat_amount);
    bond.taker_amount = Some(taker_ctx.amount);
    bond.taker_fee = Some(taker_ctx.fee);
    bond.taker_dev_fee = Some(taker_ctx.dev_fee);

    let bond = create_bond(pool, bond).await?;

    info!(
        "Bond requested: bond_id={} order_id={} role={} amount_sats={}",
        bond.id, order.id, bond.role, bond.amount_sats
    );

    // Phase 1.5: the bond bolt11 ships as a dedicated
    // `Action::PayBondInvoice` (see module-level doc). The `SmallOrder`
    // payload mirrors what the order looks like to clients on the wire,
    // so its `status` field stays `Pending` — matching the NIP-69 bucket
    // that `WaitingTakerBond` maps to in `nip33::create_status_tags`.
    let order_kind = order.get_order_kind().map_err(MostroInternalErr)?;
    let bond_small = SmallOrder::new(
        Some(order.id),
        Some(order_kind),
        Some(Status::Pending),
        amount,
        order.fiat_code.clone(),
        order.min_amount,
        order.max_amount,
        taker_ctx.fiat_amount,
        order.payment_method.clone(),
        order.premium,
        None,
        None,
        None,
        None,
        None,
    );

    // Arm the LND subscriber BEFORE shipping the bolt11 to the taker.
    // If we emit the invoice first and the taker pays before the
    // subscriber is attached, we miss the `Accepted` event and the
    // take never resumes (the HTLC eventually unwinds via CLTV but
    // the trade is dead in the meantime). On subscribe failure, undo
    // the persisted bond so we don't strand a `Requested` row with
    // no listener — and keep the invoice unsent so the taker can
    // retry the take cleanly.
    if let Err(e) = bond_invoice_subscribe(hash, request_id).await {
        warn!(
            bond_id = %bond.id,
            order_id = %bond.order_id,
            "request_taker_bond: subscribe failed ({}); rolling back bond row",
            e
        );
        // Best-effort cleanup: cancel the LND hold invoice and mark
        // the row Released. Mirrors the "always release" exit path
        // contract.
        let _ = release_bond(pool, &bond).await;
        return Err(e);
    }

    enqueue_order_msg(
        request_id,
        Some(order.id),
        Action::PayBondInvoice,
        Some(Payload::PaymentRequest(
            Some(bond_small),
            invoice_resp.payment_request,
            None,
        )),
        taker_pubkey,
        Some(taker_ctx.trade_index),
    )
    .await;

    // Phase 1.5: park the order at `WaitingTakerBond` while the bond is
    // outstanding. The bond subscriber is armed before the message is sent
    // (a few lines up), so a fast `Accepted` callback can have already
    // transitioned this order to `WaitingPayment` / `WaitingBuyerInvoice`
    // by the time we get here; a concurrent maker cancel can have moved
    // it to `Canceled`; and a sibling take from a different pubkey can
    // have already flipped it to `WaitingTakerBond` itself. We must NOT
    // blindly write back our stale `order: &Order` snapshot — that
    // would revert any of those transitions.
    //
    // Atomically claim the `Pending → WaitingTakerBond` transition with
    // a compare-and-swap UPDATE. If `rows_affected == 0`, another path
    // already owns the order's status; we skip the Nostr republish and
    // exit cleanly. If we win, we then republish the NIP-33 event and
    // patch the persisted `event_id` only (not the full row) so we
    // never clobber concurrent field updates.
    let cas = sqlx::query("UPDATE orders SET status = ? WHERE id = ? AND status = ?")
        .bind(Status::WaitingTakerBond.to_string())
        .bind(order.id)
        .bind(Status::Pending.to_string())
        .execute(pool)
        .await;
    let claimed = match &cas {
        Ok(r) => r.rows_affected() == 1,
        Err(e) => {
            warn!(
                order_id = %order.id,
                "request_taker_bond: WaitingTakerBond compare-and-swap failed: {}", e
            );
            false
        }
    };
    if claimed {
        let my_keys = get_keys()?;
        match crate::util::update_order_event(&my_keys, Status::WaitingTakerBond, order).await {
            Ok(updated) => {
                if let Err(e) = sqlx::query("UPDATE orders SET event_id = ? WHERE id = ?")
                    .bind(&updated.event_id)
                    .bind(order.id)
                    .execute(pool)
                    .await
                {
                    warn!(
                        order_id = %order.id,
                        "request_taker_bond: failed to persist event_id after WaitingTakerBond republish: {}", e
                    );
                }
            }
            Err(e) => {
                warn!(
                    order_id = %order.id,
                    "request_taker_bond: WaitingTakerBond republish failed: {}", e
                );
            }
        }
    }

    Ok(bond)
}

/// Create a hold invoice for the **maker's** bond, persist a `Bond` row
/// in `Requested`, ship the bolt11 to the maker, and arm the LND
/// subscriber that flips the row to `Locked` once the maker pays.
///
/// Phase 5 counterpart of [`request_taker_bond`]. Unlike the taker side
/// there is exactly one maker bond per order (no concurrent-bonds race),
/// and the order has already been persisted at
/// [`Status::WaitingMakerBond`] by `publish_order` with **no** NIP-33
/// event emitted — the order stays invisible in the book until the bond
/// locks. On `Accepted`, [`on_bond_invoice_accepted`] resumes the
/// deferred publication (see `crate::util::resume_publish_after_maker_bond`).
///
/// `notional_sats` is the sats notional the bond is sized against: the
/// fixed order amount for a fixed-price order, or the price-converted
/// fiat amount for a market-priced single order. Range orders never
/// reach this function in Phase 5 (deferred to Phase 6).
///
/// On any failure the bond row may exist in `Requested` with no LND
/// counterpart; the order will be reaped (and the bond released) by the
/// `WaitingMakerBond` expiry path, mirroring the taker "always release"
/// contract.
pub async fn request_maker_bond(
    pool: &Pool<Sqlite>,
    order: &Order,
    maker_pubkey: PublicKey,
    notional_sats: i64,
    request_id: Option<u64>,
    trade_index: Option<i64>,
) -> Result<Bond, MostroError> {
    let cfg = Settings::get_bond().ok_or_else(|| {
        MostroInternalErr(ServiceError::UnexpectedError(
            "anti_abuse_bond block is missing while maker bond was deemed required".into(),
        ))
    })?;

    let amount = compute_bond_amount(notional_sats, cfg);
    let memo = format!("mostro bond order_id={}", order.id);

    let mut ln_client = LndConnector::new().await?;
    let (invoice_resp, preimage, hash) = ln_client
        .create_hold_invoice(&memo, amount)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::HoldInvoiceError(e.to_string())))?;

    let mut bond = Bond::new_requested(order.id, maker_pubkey.to_string(), BondRole::Maker, amount);
    bond.hash = Some(bytes_to_string(&hash));
    bond.preimage = Some(bytes_to_string(&preimage));
    bond.payment_request = Some(invoice_resp.payment_request.clone());
    // No `taker_*` context on a maker bond: those columns describe the
    // deferred take snapshot of a concurrent taker bond and stay NULL here.

    let bond = create_bond(pool, bond).await?;

    info!(
        "Maker bond requested: bond_id={} order_id={} amount_sats={}",
        bond.id, order.id, bond.amount_sats
    );

    // The bond bolt11 ships as a dedicated `Action::PayBondInvoice`, same
    // as the taker side. The order is not on the wire yet (no NIP-33
    // event), so the `SmallOrder` carries `Status::Pending` purely as a
    // neutral placeholder for the client.
    let order_kind = order.get_order_kind().map_err(MostroInternalErr)?;
    let bond_small = SmallOrder::new(
        Some(order.id),
        Some(order_kind),
        Some(Status::Pending),
        amount,
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

    // Arm the subscriber BEFORE shipping the bolt11 (same ordering
    // rationale as the taker side: a fast payer must not race ahead of
    // the listener). On subscribe failure, release the row so we don't
    // strand a `Requested` bond with no listener.
    if let Err(e) = bond_invoice_subscribe(hash, request_id).await {
        warn!(
            bond_id = %bond.id,
            order_id = %bond.order_id,
            "request_maker_bond: subscribe failed ({}); rolling back bond row",
            e
        );
        let _ = release_bond(pool, &bond).await;
        return Err(e);
    }

    enqueue_order_msg(
        request_id,
        Some(order.id),
        Action::PayBondInvoice,
        Some(Payload::PaymentRequest(
            Some(bond_small),
            invoice_resp.payment_request,
            None,
        )),
        maker_pubkey,
        trade_index,
    )
    .await;

    Ok(bond)
}

/// Outcome of a `cancel_hold_invoice` attempt against LND, classified
/// from the structured gRPC error so the caller can decide whether the
/// HTLC is verifiably no longer encumbered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CancelOutcome {
    /// The cancel landed at LND (or didn't need to: the invoice was
    /// already canceled / never existed). The HTLC, if there ever was
    /// one, is no longer encumbered. Safe to mark `Released`.
    AlreadyDone,
    /// Transport / server error from LND, including LND being
    /// unreachable. The HTLC **may still be encumbered**. Leave the bond
    /// in its current active state so a future code path retries.
    Transient,
}

/// Classify the error returned by `LndConnector::cancel_hold_invoice`.
///
/// We rely on the `code=<grpc::Code>` prefix `cancel_hold_invoice`
/// embeds, plus message-text patterns LND emits when an invoice is
/// already canceled / unknown (those typically come back as
/// `code=Unknown` with a recognisable message, so message inspection is
/// load-bearing — not just defensive).
///
/// Anything we can't classify confidently maps to `Transient`: the
/// safer side is to delay cleanup until the next exit path or CLTV
/// expiry, never to falsely report a release on an HTLC LND still has.
fn classify_cancel_error(err: &MostroError) -> CancelOutcome {
    let s = err.to_string().to_lowercase();

    // gRPC codes that mean the cancel was idempotent / target wasn't there.
    if s.contains("code=notfound") || s.contains("code=alreadyexists") {
        return CancelOutcome::AlreadyDone;
    }
    // LND-specific message patterns that come back under `code=Unknown`.
    if s.contains("already cancelled")
        || s.contains("already canceled")
        || s.contains("unable to locate invoice")
        || s.contains("invoice not found")
        || s.contains("no such invoice")
    {
        return CancelOutcome::AlreadyDone;
    }
    // Everything else — Unavailable, DeadlineExceeded, transport errors,
    // unexpected Internal, codes we don't recognise — is conservatively
    // treated as transient. The bond stays active and gets retried on
    // the next exit path / CLTV expiry / daemon restart.
    CancelOutcome::Transient
}

/// Release a single bond: cancel the hold invoice and transition the
/// row to `Released` **only if** the HTLC is verifiably no longer
/// encumbered.
///
/// **Idempotent for terminal states.** A bond already in `Released`,
/// `Slashed`, or `Failed` is a no-op.
///
/// **Safety contract for transient LND failures.** When
/// `cancel_hold_invoice` fails with a transport / server error (LND
/// unreachable, deadline exceeded, etc.), the bond is left in its
/// current active state and the error is propagated to the caller.
/// Marking `Released` here would drop the bond out of
/// `find_active_bonds*` (which filters on `state IN ('requested',
/// 'locked')`), stranding the taker's funds in LND with no retry path
/// — the issue raised in the Phase 1 review.
///
/// The recovery path for a left-active bond is implicit:
/// - The LND subscriber spawned by `bond_invoice_subscribe` (and
///   re-attached by `resubscribe_active_bonds` on restart) catches the
///   eventual `InvoiceState::Canceled` — emitted either when LND
///   recovers and we retry, or when the hold invoice's CLTV expires
///   and LND auto-cancels — and `on_bond_invoice_canceled` then marks
///   the bond `Released`.
/// - Operators see a structured `warn` event with `bond_id`, `order_id`,
///   and the classified outcome so they can spot and intervene if a
///   bond stays stuck.
pub async fn release_bond(pool: &Pool<Sqlite>, bond: &Bond) -> Result<(), MostroError> {
    // Parse `state` once into the enum so callers don't depend on the
    // `Display` form for control flow (and a malformed value short-
    // circuits to "no-op" instead of falsely transitioning).
    let state = BondState::from_str(&bond.state).map_err(|e| {
        MostroInternalErr(ServiceError::UnexpectedError(format!(
            "Bond {} has unparseable state {:?}: {}",
            bond.id, bond.state, e
        )))
    })?;
    if state.is_terminal() {
        return Ok(());
    }

    if let Some(hash) = bond.hash.as_ref() {
        match LndConnector::new().await {
            Ok(mut ln) => {
                if let Err(e) = ln.cancel_hold_invoice(hash).await {
                    match classify_cancel_error(&e) {
                        CancelOutcome::AlreadyDone => {
                            // Common race with the subscriber, or the
                            // invoice was never created in the first place
                            // (request_taker_bond bailed before the row got
                            // a hash). HTLC is verifiably gone — fall
                            // through to mark Released.
                            info!(
                                bond_id = %bond.id,
                                order_id = %bond.order_id,
                                "cancel_hold_invoice reports already-done ({}); marking Released",
                                e
                            );
                        }
                        CancelOutcome::Transient => {
                            warn!(
                                bond_id = %bond.id,
                                order_id = %bond.order_id,
                                outcome = "transient",
                                "cancel_hold_invoice failed transiently ({}); leaving bond {} for retry",
                                e, bond.state
                            );
                            return Err(e);
                        }
                    }
                }
            }
            Err(e) => {
                // LND unreachable: definitionally transient. Don't pretend
                // the HTLC is gone.
                warn!(
                    bond_id = %bond.id,
                    order_id = %bond.order_id,
                    outcome = "transient",
                    "could not connect to LND for cancel ({}); leaving bond {} for retry",
                    e, bond.state
                );
                return Err(e);
            }
        }
    }

    let mut updated = bond.clone();
    updated.state = BondState::Released.to_string();
    updated.released_at = Some(Utc::now().timestamp());
    let id = updated.id;
    let order_id = updated.order_id;
    updated
        .update(pool)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

    info!(
        "Bond {} released for order {} (was state={})",
        id, order_id, bond.state
    );
    Ok(())
}

/// Release every active (`Requested` or `Locked`) bond attached to an
/// order. Designed to be the **single** call sites use from each exit
/// path — the lookup and the per-row release are both here.
///
/// **Not gated on `Settings::is_bond_enabled()`.** An operator can flip
/// the feature off (or remove the `[anti_abuse_bond]` block) while bonds
/// are still locked in LND from a prior enabled period; gating release
/// on the *current* config would strand those funds. The lookup is a
/// single indexed SELECT that returns zero rows for nodes that never
/// enabled the feature, so the cost of always running is negligible.
///
/// Returns `Ok(())` when no active bonds exist; never fails the caller
/// for individual bond failures (those are logged and the loop
/// continues).
pub async fn release_bonds_for_order(
    pool: &Pool<Sqlite>,
    order_id: Uuid,
) -> Result<(), MostroError> {
    release_active_bonds(pool, order_id, false).await
}

/// Release every active bond on `order_id`, optionally **retaining** the
/// maker's bond.
///
/// `retain_makers = true` is the waiting-timeout **republish** path: the
/// order returns to the book, the maker is still committed to it, so its
/// `Locked` bond must stay put and be resolved only when the order itself
/// terminates (completed, cancelled, or expired `Pending`). Only the
/// abandoning taker side is released. Every other path releases all bonds
/// (`retain_makers = false`).
async fn release_active_bonds(
    pool: &Pool<Sqlite>,
    order_id: Uuid,
    retain_makers: bool,
) -> Result<(), MostroError> {
    let bonds = find_active_bonds_for_order(pool, order_id).await?;
    let maker = BondRole::Maker.to_string();
    for bond in bonds.iter() {
        if retain_makers && bond.role == maker {
            continue;
        }
        if let Err(e) = release_bond(pool, bond).await {
            warn!("Failed to release bond {}: {}", bond.id, e);
        }
    }
    Ok(())
}

/// Best-effort release helper for the Phase 1 exit paths.
///
/// Every order-exit flow (release, cancel, admin actions, scheduler
/// timeouts) wants the same shape: try to release the bond, and on
/// failure log a warning tagged with the call site — never propagate.
/// Centralising the pattern keeps each call site to a single line and
/// guarantees consistent log structure for operators.
pub async fn release_bonds_for_order_or_warn(
    pool: &Pool<Sqlite>,
    order_id: Uuid,
    context: &'static str,
) {
    if let Err(e) = release_bonds_for_order(pool, order_id).await {
        warn!("{context}: bond release failed for {}: {}", order_id, e);
    }
}

/// Like [`release_bonds_for_order_or_warn`] but **retains the maker's
/// bond** — the waiting-timeout republish path (see [`release_active_bonds`]).
/// The maker's `Locked` bond stays put because the order returns to the
/// book with the maker still committed; only the abandoning taker side is
/// released.
pub async fn release_taker_bonds_for_order_or_warn(
    pool: &Pool<Sqlite>,
    order_id: Uuid,
    context: &'static str,
) {
    if let Err(e) = release_active_bonds(pool, order_id, true).await {
        warn!("{context}: bond release failed for {}: {}", order_id, e);
    }
}

/// Spawn the LND subscriber for a bond hold invoice. The subscriber
/// transitions the bond row through `Locked` / `Released` based on the
/// invoice state and, on `Locked`, resumes the original take flow.
///
/// Mirrors the structure of `crate::util::invoice_subscribe` so restart
/// resilience can later reuse the same shape.
pub async fn bond_invoice_subscribe(
    hash: Vec<u8>,
    request_id: Option<u64>,
) -> Result<(), MostroError> {
    let mut ln_client = LndConnector::new().await?;
    let (tx, mut rx) = channel::<InvoiceMessage>(100);

    tokio::spawn(async move {
        if let Err(e) = ln_client.subscribe_invoice(hash, tx).await {
            warn!("Bond invoice subscriber ended with error: {e}");
        }
    });

    let pool = crate::config::settings::get_db_pool();

    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            let hash_hex = bytes_to_string(msg.hash.as_ref());
            match msg.state {
                InvoiceState::Accepted => {
                    if let Err(e) = on_bond_invoice_accepted(&hash_hex, &pool, request_id).await {
                        warn!("Bond invoice accepted handler error: {e}");
                    }
                }
                InvoiceState::Canceled => {
                    if let Err(e) = on_bond_invoice_canceled(&hash_hex, &pool).await {
                        warn!("Bond invoice canceled handler error: {e}");
                    }
                }
                InvoiceState::Settled => {
                    info!("Bond hash {hash_hex}: invoice settled");
                }
                InvoiceState::Open => {
                    info!("Bond hash {hash_hex}: invoice open (waiting for payment)");
                }
            }
        }
    });

    Ok(())
}

/// Restart hook: re-subscribe to every bond that was still active when
/// the daemon stopped. Called from `main` next to `find_held_invoices`.
///
/// Like [`release_bonds_for_order`], this is **not gated on the current
/// feature flag**: bonds locked under a previous enabled period must
/// continue to flow through state transitions even after an operator
/// disables the feature, otherwise their hold invoices stay stranded
/// in LND.
pub async fn resubscribe_active_bonds(pool: &Arc<Pool<Sqlite>>) -> Result<(), MostroError> {
    let bonds = find_active_bonds(pool.as_ref()).await?;
    for bond in bonds.into_iter() {
        if let Some(hash) = bond.hash.as_ref() {
            // Hex string back to bytes for LND.
            match Vec::<u8>::from_hex(hash) {
                Ok(bytes) => {
                    if let Err(e) = bond_invoice_subscribe(bytes, None).await {
                        warn!("Failed to resubscribe bond {}: {}", bond.id, e);
                    } else {
                        info!("Resubscribed bond {} (state={})", bond.id, bond.state);
                    }
                }
                Err(e) => warn!("Bond {} has malformed hash: {}", bond.id, e),
            }
        }
    }
    Ok(())
}

/// Subscriber callback for `InvoiceState::Accepted`: a taker has just
/// locked their bond hold invoice.
///
/// Under concurrent taker bonds N prospective takers may have
/// outstanding `Requested` rows on the same order. The first bond to
/// reach `Locked` wins; this function is the chokepoint that decides
/// the winner and tears down the losers.
///
/// Algorithm:
/// 1. Atomically attempt `Requested → Locked` for this bond, **guarded
///    by `NOT EXISTS (… another bond on the same order already
///    Locked)`**. Exactly one concurrent firing can win per order; if
///    two `Accepted` events arrive in the same window, only one
///    UPDATE will affect a row.
/// 2. On `rows_affected == 0`, re-read the bond's current state to
///    distinguish a *duplicate firing for the already-`Locked` bond*
///    (LND reconnect / restart resubscriber, where we should fall
///    through and retry the resume) from a *lost race* (another bond
///    on this order locked first, where we cancel our own HTLC,
///    notify our taker, and exit).
/// 3. On `rows_affected == 1` (we won), iterate every other still-
///    `Requested` bond on the order, release each (cancels the LND
///    hold invoice + marks `Released`), and message `Action::Canceled`
///    to each losing taker.
/// 4. Copy the winning bond's `taker_*` columns onto the order row
///    (pubkeys, identity, trade index, per-bond pricing, optional
///    buyer invoice), then call `resume_take_after_bond` to drive
///    the take into the trade-flow status.
///
/// The state transition and the resume are decoupled (as before), so a
/// transient resume failure (LND/DB/Nostr blip while creating the
/// trade hold invoice) doesn't leave the order stuck — the next
/// `Accepted` redelivery, or the restart resubscriber, retries the
/// continuation as long as the bond is still `Locked` and the order is
/// still `Pending`.
async fn on_bond_invoice_accepted(
    hash: &str,
    pool: &Pool<Sqlite>,
    request_id: Option<u64>,
) -> Result<(), MostroError> {
    let bond = match find_bond_by_hash(pool, hash).await? {
        Some(b) => b,
        None => {
            warn!("Bond invoice accepted for unknown hash {hash}");
            return Ok(());
        }
    };

    // Phase 5: a maker bond is a singleton (one per order, posted at
    // order-creation time), so it never participates in the taker
    // first-to-lock-wins race below. Route it to its own lock + resume
    // path, which finishes the deferred order publication.
    if bond.role == BondRole::Maker.to_string() {
        return on_maker_bond_accepted(&bond, hash, pool, request_id).await;
    }

    // Atomic Requested → Locked with concurrent-bonds guard. Exactly
    // one bond can win per order — if two `Accepted` events arrive in
    // the same window, the loser's UPDATE returns `rows_affected = 0`.
    //
    // Phase 5: the guard counts only OTHER `Locked` *taker* bonds. The
    // first-to-lock-wins race is purely a taker concern; under
    // `apply_to = both` the maker's own bond is already `Locked` on every
    // published order (that is the steady state, not a competitor). Without
    // the `role = 'taker'` filter the `NOT EXISTS` subquery would see the
    // maker bond, this UPDATE would affect zero rows, and the first taker
    // to pay would be wrongly treated as a race loser — rejecting every
    // taker on a maker-bonded order.
    let now = Utc::now().timestamp();
    let result = sqlx::query(
        "UPDATE bonds SET state = ?, locked_at = ? \
         WHERE id = ? AND state = ? \
           AND NOT EXISTS ( \
             SELECT 1 FROM bonds b2 \
             WHERE b2.order_id = ? AND b2.state = ? AND b2.role = ? AND b2.id != ? \
           )",
    )
    .bind(BondState::Locked.to_string())
    .bind(now)
    .bind(bond.id)
    .bind(BondState::Requested.to_string())
    .bind(bond.order_id)
    .bind(BondState::Locked.to_string())
    .bind(BondRole::Taker.to_string())
    .bind(bond.id)
    .execute(pool)
    .await
    .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

    // Re-read the bond so a concurrent release (Locked → Released) is
    // visible.
    let current = match find_bond_by_hash(pool, hash).await? {
        Some(b) => b,
        None => return Ok(()),
    };
    let current_state = match BondState::from_str(&current.state) {
        Ok(s) => s,
        Err(e) => {
            warn!(
                "Bond {} has unparseable state {:?}: {} — skipping resume",
                current.id, current.state, e
            );
            return Ok(());
        }
    };

    if result.rows_affected() == 0 && current_state != BondState::Locked {
        // We did not win the lock. Either another bond on this order
        // beat us to it, or our row was released between the UPDATE
        // and the re-read. Cancel our own HTLC, notify our taker, and
        // exit. `release_bond` is idempotent for terminal states, so
        // safe even if a parallel path already marked us Released.
        info!(
            "Bond {} lost concurrent-bonds race (current state={}) — releasing and notifying taker",
            current.id, current.state
        );
        if !current_state.is_terminal() {
            if let Err(e) = release_bond(pool, &current).await {
                warn!(
                    bond_id = %current.id,
                    "release_bond on race-loser failed: {}", e
                );
            }
        }
        notify_loser(&current).await;
        return Ok(());
    }

    if result.rows_affected() == 1 {
        info!("Bond {} locked for order {}", bond.id, bond.order_id);

        // We just won. Tear down every other still-`Requested` bond on
        // this order: cancel the LND hold invoice (so the loser
        // taker's funds aren't held) and message them `Action::Canceled`.
        let losers = match find_active_bonds_for_order(pool, current.order_id).await {
            Ok(rows) => rows,
            Err(e) => {
                warn!(
                    order_id = %current.order_id,
                    "could not enumerate losing bonds after lock: {}", e
                );
                Vec::new()
            }
        };
        for loser in losers
            .iter()
            .filter(|b| b.id != current.id && b.state == BondState::Requested.to_string())
        {
            if let Err(e) = release_bond(pool, loser).await {
                warn!(
                    bond_id = %loser.id,
                    "failed to release losing concurrent bond: {}", e
                );
            }
            notify_loser(loser).await;
        }
    }

    // current_state == Locked at this point: either we just won, or a
    // duplicate firing for our already-locked bond. Fall through to
    // the resume retry.
    if current_state != BondState::Locked {
        return Ok(());
    }

    let order = Order::by_id(pool, current.order_id)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?
        .ok_or_else(|| {
            MostroInternalErr(ServiceError::UnexpectedError(format!(
                "Bond {} references missing order {}",
                current.id, current.order_id
            )))
        })?;

    // Defense-in-depth: only drive the take forward when the order is
    // still in a pre-trade state we left it in. Phase 1.5 parks the
    // order at `WaitingTakerBond` during the bond window; Phase 1 left
    // it at `Pending`. Both are valid pre-trade entry points. If the
    // order has already moved on (resume succeeded on a previous
    // firing, or maker / admin / scheduler canceled), do not re-trigger
    // the take.
    if order.status != Status::Pending.to_string()
        && order.status != Status::WaitingTakerBond.to_string()
    {
        info!(
            "Bond {} accepted but order {} is in status {} — skipping resume",
            current.id, order.id, order.status
        );
        return Ok(());
    }

    // Promote the winning bond's `taker_*` context onto the order row.
    // Under concurrent bonds the take handler deliberately did not
    // touch these fields (so racing takers couldn't clobber each
    // other); now that we know the winner, copy their snapshot.
    let order = promote_taker_context_to_order(pool, order, &current).await?;

    let my_keys = get_keys()?;
    resume_take_after_bond(pool, order, &my_keys, request_id).await
}

/// Subscriber callback path for a **maker** bond reaching `Accepted`.
///
/// The maker bond is a singleton, so there is no first-to-lock-wins race
/// and no loser to cancel. We atomically flip `Requested → Locked`, then
/// — if the order is still parked at `WaitingMakerBond` — resume the
/// deferred order publication that `publish_order` skipped.
///
/// Idempotent across redeliveries and the restart resubscriber: a
/// duplicate firing for an already-`Locked` bond re-reads `Locked` and
/// falls through to the resume, which itself no-ops once the order has
/// moved on to `Pending` (already published).
async fn on_maker_bond_accepted(
    bond: &Bond,
    hash: &str,
    pool: &Pool<Sqlite>,
    request_id: Option<u64>,
) -> Result<(), MostroError> {
    let now = Utc::now().timestamp();
    let result =
        sqlx::query("UPDATE bonds SET state = ?, locked_at = ? WHERE id = ? AND state = ?")
            .bind(BondState::Locked.to_string())
            .bind(now)
            .bind(bond.id)
            .bind(BondState::Requested.to_string())
            .execute(pool)
            .await
            .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

    // Re-read so a concurrent release (e.g. the order expired and its
    // bond was cancelled) is visible before we try to publish.
    let current = match find_bond_by_hash(pool, hash).await? {
        Some(b) => b,
        None => return Ok(()),
    };
    let current_state = match BondState::from_str(&current.state) {
        Ok(s) => s,
        Err(e) => {
            warn!(
                "Maker bond {} has unparseable state {:?}: {} — skipping publish",
                current.id, current.state, e
            );
            return Ok(());
        }
    };
    if current_state != BondState::Locked {
        info!(
            "Maker bond {} no longer Locked (state={}) — skipping publish",
            current.id, current.state
        );
        return Ok(());
    }
    if result.rows_affected() == 1 {
        info!(
            "Maker bond {} locked for order {}",
            current.id, current.order_id
        );
    }

    let order = Order::by_id(pool, current.order_id)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?
        .ok_or_else(|| {
            MostroInternalErr(ServiceError::UnexpectedError(format!(
                "Maker bond {} references missing order {}",
                current.id, current.order_id
            )))
        })?;

    // Only resume the deferred publication while the order is still
    // parked at `WaitingMakerBond`. A previous firing (or the restart
    // resubscriber) may already have published it (status `Pending`),
    // in which case we must not re-publish or re-ack the maker.
    if order.status != Status::WaitingMakerBond.to_string() {
        info!(
            "Maker bond {} accepted but order {} is in status {} — skipping publish",
            current.id, order.id, order.status
        );
        return Ok(());
    }

    let my_keys = get_keys()?;
    crate::util::resume_publish_after_maker_bond(pool, &my_keys, order, request_id).await
}

/// Message the taker of a losing concurrent bond that their take was
/// cancelled because another taker locked their bond first.
async fn notify_loser(bond: &Bond) {
    if let Ok(taker_pk) = PublicKey::from_str(&bond.pubkey) {
        enqueue_order_msg(
            None,
            Some(bond.order_id),
            Action::Canceled,
            None,
            taker_pk,
            None,
        )
        .await;
    }
}

/// Copy the winning bond's deferred taker context onto the order row.
///
/// Called from `on_bond_invoice_accepted` once a bond wins the
/// `Requested → Locked` race. The take handler did not touch the
/// order's taker fields at take-time (under concurrent bonds the
/// "owner" is undefined until the lock), so we populate them here
/// from the bond's `taker_*` columns and persist before
/// `resume_take_after_bond` reads them back via
/// `order.get_buyer_pubkey()` / `order.get_seller_pubkey()`.
async fn promote_taker_context_to_order(
    pool: &Pool<Sqlite>,
    mut order: Order,
    bond: &Bond,
) -> Result<Order, MostroError> {
    let kind = order.get_order_kind().map_err(MostroInternalErr)?;
    match kind {
        mostro_core::order::Kind::Buy => {
            // Taker is the seller side of a buy order.
            order.seller_pubkey = Some(bond.pubkey.clone());
            order.master_seller_pubkey = bond.taker_identity.clone();
            order.trade_index_seller = bond.taker_trade_index;
        }
        mostro_core::order::Kind::Sell => {
            // Taker is the buyer side of a sell order.
            order.buyer_pubkey = Some(bond.pubkey.clone());
            order.master_buyer_pubkey = bond.taker_identity.clone();
            order.trade_index_buyer = bond.taker_trade_index;
            order.buyer_invoice = bond.taker_invoice.clone();
        }
    }
    if let Some(v) = bond.taker_fiat_amount {
        order.fiat_amount = v;
    }
    if let Some(v) = bond.taker_amount {
        order.amount = v;
    }
    if let Some(v) = bond.taker_fee {
        order.fee = v;
    }
    if let Some(v) = bond.taker_dev_fee {
        order.dev_fee = v;
    }
    order.set_timestamp_now();
    order
        .update(pool)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))
}

/// Subscriber callback for `InvoiceState::Canceled`: bond never locked
/// (taker abandoned the invoice, LND auto-canceled on expiration, or
/// the bond was cancelled by `release_bond` because another concurrent
/// taker locked first).
///
/// Marks the bond `Released`. Under Phase 1.5, if no other active bond
/// remains on the order and the order is in `WaitingTakerBond`, the
/// status flips back to `Pending` and is republished so the orderbook
/// reflects the empty-bond state. If other bonds are still racing,
/// the order stays in `WaitingTakerBond` (its NIP-69 wire bucket
/// remains `pending` either way).
async fn on_bond_invoice_canceled(hash: &str, pool: &Pool<Sqlite>) -> Result<(), MostroError> {
    let bond = match find_bond_by_hash(pool, hash).await? {
        Some(b) => b,
        None => return Ok(()),
    };

    if BondState::from_str(&bond.state)
        .map(|s| s.is_terminal())
        .unwrap_or(false)
    {
        return Ok(());
    }

    let mut updated = bond.clone();
    updated.state = BondState::Released.to_string();
    updated.released_at = Some(Utc::now().timestamp());
    updated
        .update(pool)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

    info!(
        "Bond {} marked Released after LND cancel (order {})",
        bond.id, bond.order_id
    );

    // Phase 1.5: if this was the last active bond on the order and the
    // order is parked at `WaitingTakerBond`, flip it back to `Pending`
    // and republish so the orderbook reflects "no taker mid-bond".
    // Other active bonds (e.g. a Locked winner mid-resume, or a fresh
    // concurrent taker) keep the order at `WaitingTakerBond` — their
    // own paths own the next transition. Errors are warn-logged but
    // do not propagate: the bond is already marked Released, which is
    // the load-bearing invariant of this callback.
    if let Err(e) = maybe_drop_waiting_taker_bond(pool, bond.order_id).await {
        warn!(
            order_id = %bond.order_id,
            "on_bond_invoice_canceled: failed to flip status back to Pending: {}", e
        );
    }
    Ok(())
}

/// Atomically transition `order_id` from `WaitingTakerBond` back to
/// `Pending` **iff** no active taker bond remains on it. Returns `true`
/// when the transition was applied, `false` when it was a no-op.
///
/// This is the load-bearing, side-effect-free core of
/// [`maybe_drop_waiting_taker_bond`] (the latter adds the NIP-33
/// republish, which needs process-wide keys). Splitting it out lets the
/// CAS semantics be unit-tested directly instead of via an inlined SQL
/// copy that could drift from production.
///
/// Race-free: the status check, active-bond check, and status update
/// run in a single conditional `UPDATE … WHERE … AND NOT EXISTS (…)`
/// statement. A concurrent `on_bond_invoice_accepted` that flips a
/// bond to `Locked` between our last check and the write would make
/// the `NOT EXISTS` clause false → `rows_affected == 0` → we skip
/// the republish. Likewise a concurrent transition out of
/// `WaitingTakerBond` (winner promotes to `WaitingPayment`, maker
/// cancels, etc.) is caught by the `status = 'waiting-taker-bond'`
/// predicate.
///
/// Phase 5: the active-bond check is scoped to `role = 'taker'`. Under
/// `apply_to = both` the order carries a `Locked` *maker* bond for the
/// whole trade; counting it here would make `NOT EXISTS` permanently
/// false, so a `WaitingTakerBond` order whose last taker bond was just
/// cancelled would never drop back to `Pending`. The drop-to-Pending
/// decision depends solely on whether any *taker* is still racing.
pub(crate) async fn drop_waiting_taker_bond_to_pending(
    pool: &Pool<Sqlite>,
    order_id: Uuid,
) -> Result<bool, MostroError> {
    let cas = sqlx::query(
        "UPDATE orders SET status = ? \
         WHERE id = ? AND status = ? \
           AND NOT EXISTS ( \
             SELECT 1 FROM bonds \
             WHERE order_id = ? AND state IN (?, ?) AND role = ? \
           )",
    )
    .bind(Status::Pending.to_string())
    .bind(order_id)
    .bind(Status::WaitingTakerBond.to_string())
    .bind(order_id)
    .bind(BondState::Requested.to_string())
    .bind(BondState::Locked.to_string())
    .bind(BondRole::Taker.to_string())
    .execute(pool)
    .await
    .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

    Ok(cas.rows_affected() == 1)
}

/// If `order_id` is currently in `Status::WaitingTakerBond` and has no
/// remaining active taker bond, transition it back to `Status::Pending`
/// and republish the NIP-33 event. No-op otherwise.
///
/// Used by `on_bond_invoice_canceled` (when LND cancels the only
/// outstanding bond's hold invoice) and by the taker self-cancel path
/// in `cancel.rs` (when the sender was the last bonded taker). Both
/// call sites need the same "drop back to Pending if empty" logic;
/// extracting it keeps them consistent. The CAS itself lives in
/// [`drop_waiting_taker_bond_to_pending`]; this wrapper adds the
/// NIP-33 republish.
pub(crate) async fn maybe_drop_waiting_taker_bond(
    pool: &Pool<Sqlite>,
    order_id: Uuid,
) -> Result<(), MostroError> {
    if !drop_waiting_taker_bond_to_pending(pool, order_id).await? {
        // Either the order is no longer in `WaitingTakerBond`
        // (winner / maker-cancel / admin already moved it on), or a
        // concurrent taker bond is still racing. Either way we have no
        // status transition to publish.
        return Ok(());
    }

    // We won the transition. Republish the NIP-33 event so the
    // orderbook reflects the new status. `update_order_event` re-reads
    // the current row state via the supplied `&Order`, so we fetch a
    // fresh snapshot first to avoid sending tags built from data that
    // changed mid-flight under us. Errors are non-fatal: the DB is
    // already at `Pending`, and the next genuine transition will
    // refresh the published event.
    let fresh = match Order::by_id(pool, order_id)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?
    {
        Some(o) => o,
        None => return Ok(()),
    };
    let my_keys = get_keys()?;
    match crate::util::update_order_event(&my_keys, Status::Pending, &fresh).await {
        Ok(updated) => {
            if let Err(e) = sqlx::query("UPDATE orders SET event_id = ? WHERE id = ?")
                .bind(&updated.event_id)
                .bind(order_id)
                .execute(pool)
                .await
            {
                warn!(
                    order_id = %order_id,
                    "maybe_drop_waiting_taker_bond: failed to persist event_id after Pending republish: {}", e
                );
            }
        }
        Err(e) => {
            warn!(
                order_id = %order_id,
                "maybe_drop_waiting_taker_bond: Pending republish failed: {}", e
            );
        }
    }
    info!(
        "Order {} dropped back to Pending after last bond released",
        order_id
    );
    Ok(())
}

/// Resume the take flow after the winning bond locks.
///
/// The take handler deferred the trade hold-invoice step under
/// concurrent bonds; the winning bond's `taker_*` columns have just
/// been promoted onto the order by `promote_taker_context_to_order`,
/// so the order now has its `buyer_pubkey` / `seller_pubkey` /
/// `buyer_invoice` / per-take pricing in place and we can drive the
/// trade flow exactly as the legacy path would have.
async fn resume_take_after_bond(
    pool: &Pool<Sqlite>,
    mut order: Order,
    my_keys: &Keys,
    request_id: Option<u64>,
) -> Result<(), MostroError> {
    let kind = order.get_order_kind().map_err(MostroInternalErr)?;
    let buyer_pubkey = order.get_buyer_pubkey().map_err(MostroInternalErr)?;
    let seller_pubkey = order.get_seller_pubkey().map_err(MostroInternalErr)?;

    match kind {
        // Buy order → taker = seller, no buyer-invoice required up front:
        // mirror the post-take path in take_buy_action.
        mostro_core::order::Kind::Buy => {
            show_hold_invoice(
                my_keys,
                None,
                &buyer_pubkey,
                &seller_pubkey,
                order,
                request_id,
            )
            .await
        }
        // Sell order → taker = buyer. If the buyer included an invoice in
        // the take message we already persisted it on `order.buyer_invoice`;
        // otherwise we ask for one. This mirrors take_sell_action.
        mostro_core::order::Kind::Sell => {
            if order.buyer_invoice.is_some() {
                let payment_request = order.buyer_invoice.clone();
                show_hold_invoice(
                    my_keys,
                    payment_request,
                    &buyer_pubkey,
                    &seller_pubkey,
                    order,
                    request_id,
                )
                .await
            } else {
                set_waiting_invoice_status(&mut order, buyer_pubkey, request_id)
                    .await
                    .map_err(|_| MostroInternalErr(ServiceError::UpdateOrderStatusError))?;
                let order_updated =
                    crate::util::update_order_event(my_keys, Status::WaitingBuyerInvoice, &order)
                        .await
                        .map_err(|e| MostroInternalErr(ServiceError::NostrError(e.to_string())))?;
                order_updated
                    .update(pool)
                    .await
                    .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::bond::types::BondRole;
    use sqlx::sqlite::SqlitePoolOptions;

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
        // dev_fee columns were added by a later migration but
        // `Order::by_id` SELECTs them. Apply each ALTER as a separate
        // statement (sqlx::query treats the whole string as one).
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
        // cashu escrow columns (mostro-core 0.12.1) — `Order::by_id` SELECTs
        // them. Apply each ALTER separately for the same reason as dev_fee.
        for stmt in include_str!("../../../migrations/20260530120000_cashu_escrow_fields.sql")
            .split(';')
            .map(str::trim)
            .filter(|s| !s.is_empty() && !s.lines().all(|l| l.trim_start().starts_with("--")))
        {
            sqlx::query(stmt)
                .execute(&pool)
                .await
                .expect("cashu escrow migration");
        }
        pool
    }

    async fn insert_order(pool: &Pool<Sqlite>, id: Uuid) {
        sqlx::query(
            r#"INSERT INTO orders (
                id, kind, event_id, status, premium, payment_method,
                amount, fiat_code, fiat_amount, created_at, expires_at
            ) VALUES (?, 'buy', ?, 'pending', 0, 'ln', 1000, 'USD', 10, 0, 0)"#,
        )
        .bind(id)
        .bind(id.simple().to_string())
        .execute(pool)
        .await
        .expect("insert order");
    }

    fn make_bond(order_id: Uuid, state: BondState) -> Bond {
        let mut b = Bond::new_requested(order_id, "a".repeat(64), BondRole::Taker, 1_500);
        b.state = state.to_string();
        b.hash = Some("c".repeat(64));
        b
    }

    /// Mirror of the production taker first-to-lock-wins CAS in
    /// `on_bond_invoice_accepted` (including the `role = 'taker'` filter on
    /// the `NOT EXISTS` guard). Returns `rows_affected`. Kept in lockstep
    /// with the real query so the race tests verify the actual semantics.
    async fn try_lock(pool: &Pool<Sqlite>, bond: &Bond) -> u64 {
        sqlx::query(
            "UPDATE bonds SET state = ?, locked_at = ? \
             WHERE id = ? AND state = ? \
               AND NOT EXISTS ( \
                 SELECT 1 FROM bonds b2 \
                 WHERE b2.order_id = ? AND b2.state = ? AND b2.role = ? AND b2.id != ? \
               )",
        )
        .bind(BondState::Locked.to_string())
        .bind(Utc::now().timestamp())
        .bind(bond.id)
        .bind(BondState::Requested.to_string())
        .bind(bond.order_id)
        .bind(BondState::Locked.to_string())
        .bind(BondRole::Taker.to_string())
        .bind(bond.id)
        .execute(pool)
        .await
        .unwrap()
        .rows_affected()
    }

    #[tokio::test]
    async fn release_bond_is_idempotent_for_terminal_states() {
        let pool = setup_pool().await;
        let order_id = Uuid::new_v4();
        insert_order(&pool, order_id).await;
        let bond = create_bond(&pool, make_bond(order_id, BondState::Released))
            .await
            .unwrap();

        // No LND, no panic: idempotent on terminal states.
        release_bond(&pool, &bond).await.unwrap();

        let after = find_bond_by_hash(&pool, &"c".repeat(64))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after.state, "released");
        assert_eq!(after.released_at, bond.released_at);
    }

    #[tokio::test]
    async fn release_bonds_for_order_runs_regardless_of_feature_flag() {
        // No `[anti_abuse_bond]` block in test settings → feature off.
        // Even so, an outstanding bond row from a prior enabled period
        // MUST still be released; otherwise an operator who toggles the
        // feature off strands taker funds in LND.
        let pool = setup_pool().await;
        let order_id = Uuid::new_v4();
        insert_order(&pool, order_id).await;
        // Use a hash-less Requested bond so release_bond skips LND in
        // the unit-test harness (no Lightning settings configured).
        let mut bond = make_bond(order_id, BondState::Requested);
        bond.hash = None;
        create_bond(&pool, bond).await.unwrap();

        release_bonds_for_order(&pool, order_id).await.unwrap();

        let active = find_active_bonds_for_order(&pool, order_id).await.unwrap();
        assert!(
            active.is_empty(),
            "bond must be released even with feature disabled"
        );
    }

    #[tokio::test]
    async fn release_bond_without_hash_marks_released() {
        // A `Requested` bond with no hash yet (e.g. failure between
        // `new_requested` and `create_hold_invoice`) must still be
        // releasable: the row transitions to `Released` and no LND call
        // is attempted.
        let pool = setup_pool().await;
        let order_id = Uuid::new_v4();
        insert_order(&pool, order_id).await;
        let mut bond = make_bond(order_id, BondState::Requested);
        bond.hash = None;
        let bond = create_bond(&pool, bond).await.unwrap();

        release_bond(&pool, &bond).await.unwrap();

        let active = find_active_bonds_for_order(&pool, order_id).await.unwrap();
        assert!(active.is_empty(), "bond should no longer be active");
    }

    #[test]
    fn taker_bond_required_is_false_without_config() {
        // No global config initialized in unit tests → gate must be off.
        // Guarantees that all bond touchpoints are inert in the absence
        // of an `[anti_abuse_bond]` block.
        assert!(!taker_bond_required());
    }

    #[test]
    fn maker_bond_required_is_false_without_config() {
        // Phase 5: same inertness guarantee for the maker gate. With no
        // `[anti_abuse_bond]` block, `publish_order` must never park an
        // order at `WaitingMakerBond` or request a maker bond.
        assert!(!maker_bond_required());
    }

    #[test]
    fn locked_maker_bond_does_not_commit_the_trade() {
        // Phase 5 (load-bearing): under `apply_to = both` every published
        // order already carries a `Locked` maker bond. The take handlers'
        // committed-trade gate must NOT count it — otherwise the first
        // taker is wrongly rejected with `PendingOrderExists` on every
        // bond-enabled order. Only a `Locked` *taker* bond commits the
        // trade (first-to-lock-wins, §6.5).
        let order_id = Uuid::new_v4();
        let mut maker = Bond::new_requested(order_id, "a".repeat(64), BondRole::Maker, 1_000);
        maker.state = BondState::Locked.to_string();
        // A still-racing taker bond (Requested) must also not commit.
        let taker_requested = Bond::new_requested(order_id, "b".repeat(64), BondRole::Taker, 1_000);

        assert!(
            !trade_committed_by_locked_taker_bond(&[maker.clone(), taker_requested.clone()]),
            "a Locked maker bond + a Requested taker bond must not commit the trade"
        );

        // Once a taker bond reaches Locked, the trade IS committed.
        let mut taker_locked = taker_requested;
        taker_locked.state = BondState::Locked.to_string();
        assert!(
            trade_committed_by_locked_taker_bond(&[maker, taker_locked]),
            "a Locked taker bond commits the trade"
        );
    }

    #[test]
    fn empty_bond_set_does_not_commit_the_trade() {
        // Defensive: the no-bonds case (feature just enabled, or all
        // bonds released) must read as "not committed" so takes proceed.
        assert!(!trade_committed_by_locked_taker_bond(&[]));
    }

    #[tokio::test]
    async fn maker_bond_lock_is_singleton_and_idempotent() {
        // Phase 5: the maker bond is a singleton, so `on_maker_bond_accepted`
        // uses a plain `Requested → Locked` CAS (no concurrent-bonds
        // `NOT EXISTS` guard). The first firing locks it; a duplicate
        // firing (LND redelivery / restart resubscriber) affects zero
        // rows because the row is no longer `Requested`, and the bond
        // stays `Locked` exactly once.
        let pool = setup_pool().await;
        let order_id = Uuid::new_v4();
        insert_order(&pool, order_id).await;

        let mut bond = Bond::new_requested(order_id, "d".repeat(64), BondRole::Maker, 1_000);
        bond.hash = Some("e".repeat(64));
        let bond = create_bond(&pool, bond).await.unwrap();

        async fn try_lock(pool: &Pool<Sqlite>, bond: &Bond) -> u64 {
            sqlx::query("UPDATE bonds SET state = ?, locked_at = ? WHERE id = ? AND state = ?")
                .bind(BondState::Locked.to_string())
                .bind(Utc::now().timestamp())
                .bind(bond.id)
                .bind(BondState::Requested.to_string())
                .execute(pool)
                .await
                .unwrap()
                .rows_affected()
        }

        assert_eq!(try_lock(&pool, &bond).await, 1, "first lock wins");
        assert_eq!(
            try_lock(&pool, &bond).await,
            0,
            "duplicate firing is a no-op"
        );

        let after = find_bond_by_hash(&pool, &"e".repeat(64))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after.state, BondState::Locked.to_string());
        assert_eq!(after.role, BondRole::Maker.to_string());
    }

    #[tokio::test]
    async fn maker_bond_lock_does_not_touch_other_bonds() {
        // The maker lock CAS is keyed by `id`, so it must flip only the
        // maker row even when an unrelated bond row exists on the same
        // order. Guards against a future refactor accidentally widening
        // the predicate.
        let pool = setup_pool().await;
        let order_id = Uuid::new_v4();
        insert_order(&pool, order_id).await;

        let mut maker = Bond::new_requested(order_id, "d".repeat(64), BondRole::Maker, 1_000);
        maker.hash = Some("e".repeat(64));
        let maker = create_bond(&pool, maker).await.unwrap();

        let mut other = Bond::new_requested(order_id, "f".repeat(64), BondRole::Taker, 1_000);
        other.hash = Some("0".repeat(64));
        let other = create_bond(&pool, other).await.unwrap();

        sqlx::query("UPDATE bonds SET state = ?, locked_at = ? WHERE id = ? AND state = ?")
            .bind(BondState::Locked.to_string())
            .bind(Utc::now().timestamp())
            .bind(maker.id)
            .bind(BondState::Requested.to_string())
            .execute(&pool)
            .await
            .unwrap();

        let other_after = find_bond_by_hash(&pool, &"0".repeat(64))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            other_after.state,
            BondState::Requested.to_string(),
            "unrelated bond must stay Requested"
        );
        assert_eq!(other_after.id, other.id);
    }

    #[tokio::test]
    async fn lock_race_guard_admits_only_one_winner() {
        // With bonds A and B both Requested on the same order, the
        // first conditional UPDATE that runs flips A to Locked; the
        // second UPDATE for B sees the `NOT EXISTS … Locked` guard
        // fail and affects zero rows. This is the concurrent-bonds
        // chokepoint: exactly one bond per order may transition to
        // Locked.
        let pool = setup_pool().await;
        let order_id = Uuid::new_v4();
        insert_order(&pool, order_id).await;

        let mut a = make_bond(order_id, BondState::Requested);
        a.hash = Some("a".repeat(64));
        a.pubkey = "a".repeat(64);
        let bond_a = create_bond(&pool, a).await.unwrap();

        let mut b = make_bond(order_id, BondState::Requested);
        b.hash = Some("b".repeat(64));
        b.pubkey = "b".repeat(64);
        let bond_b = create_bond(&pool, b).await.unwrap();

        // A goes first and wins.
        assert_eq!(try_lock(&pool, &bond_a).await, 1);
        // B's UPDATE sees A already Locked → guarded out.
        assert_eq!(try_lock(&pool, &bond_b).await, 0);

        let active = find_active_bonds_for_order(&pool, order_id).await.unwrap();
        let states: Vec<_> = active.iter().map(|b| (b.id, b.state.clone())).collect();
        assert!(states
            .iter()
            .any(|(id, s)| *id == bond_a.id && s == &BondState::Locked.to_string()));
        assert!(states
            .iter()
            .any(|(id, s)| *id == bond_b.id && s == &BondState::Requested.to_string()));
    }

    #[tokio::test]
    async fn locked_maker_bond_does_not_block_taker_lock_race() {
        // Phase 5 regression (apply_to = both): a `Locked` maker bond is
        // present on every published order. The taker first-to-lock-wins
        // CAS must IGNORE it (its `NOT EXISTS` guard is scoped to
        // `role = 'taker'`), so the first taker to pay still wins. Without
        // the role filter the maker bond would satisfy the guard, the
        // taker's UPDATE would affect zero rows, and every taker would be
        // wrongly rejected as a race loser.
        let pool = setup_pool().await;
        let order_id = Uuid::new_v4();
        insert_order(&pool, order_id).await;

        // Maker bond already Locked (the steady state after publication).
        let mut maker = Bond::new_requested(order_id, "m".repeat(64), BondRole::Maker, 1_000);
        maker.state = BondState::Locked.to_string();
        maker.hash = Some("d".repeat(64));
        let maker = create_bond(&pool, maker).await.unwrap();

        // A taker now pays their bond.
        let mut taker = make_bond(order_id, BondState::Requested);
        taker.pubkey = "t".repeat(64);
        taker.hash = Some("e".repeat(64));
        let taker = create_bond(&pool, taker).await.unwrap();

        assert_eq!(
            try_lock(&pool, &taker).await,
            1,
            "taker must win the lock race despite the Locked maker bond"
        );

        // Maker bond untouched; taker bond now Locked.
        let maker_after = find_bond_by_hash(&pool, &"d".repeat(64))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(maker_after.id, maker.id);
        assert_eq!(maker_after.state, BondState::Locked.to_string());
        let taker_after = find_bond_by_hash(&pool, &"e".repeat(64))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(taker_after.id, taker.id);
        assert_eq!(taker_after.state, BondState::Locked.to_string());
    }

    #[tokio::test]
    async fn maybe_drop_waiting_taker_bond_ignores_locked_maker_bond() {
        // Phase 5 regression (apply_to = both): when the last taker bond is
        // cancelled, the order must drop from `WaitingTakerBond` back to
        // `Pending` even though the maker's `Locked` bond is still on the
        // order. The CAS's active-bond check is scoped to `role = 'taker'`,
        // so the lingering maker bond does not pin the order in
        // `WaitingTakerBond`. Exercises the real CAS helper
        // (`drop_waiting_taker_bond_to_pending`) — not an inlined SQL copy
        // — so the test tracks production semantics.
        let pool = setup_pool().await;
        let order_id = Uuid::new_v4();
        insert_order(&pool, order_id).await;
        sqlx::query("UPDATE orders SET status = ? WHERE id = ?")
            .bind(Status::WaitingTakerBond.to_string())
            .bind(order_id)
            .execute(&pool)
            .await
            .unwrap();

        // Only a Locked maker bond remains (the taker bond was already
        // released by the caller before maybe_drop runs).
        let mut maker = Bond::new_requested(order_id, "m".repeat(64), BondRole::Maker, 1_000);
        maker.state = BondState::Locked.to_string();
        maker.hash = Some("d".repeat(64));
        create_bond(&pool, maker).await.unwrap();

        let dropped = drop_waiting_taker_bond_to_pending(&pool, order_id)
            .await
            .unwrap();
        assert!(
            dropped,
            "order must drop to Pending: the Locked maker bond must not count as an active taker bond"
        );

        let status: String = sqlx::query_scalar("SELECT status FROM orders WHERE id = ?")
            .bind(order_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(status, Status::Pending.to_string());
    }

    #[tokio::test]
    async fn drop_waiting_taker_bond_held_by_active_taker_bond() {
        // Counterpart to the maker-bond test: a still-active *taker* bond
        // (Requested) MUST pin the order in `WaitingTakerBond` — the CAS
        // returns false and the status is unchanged. Confirms the role
        // filter does not over-drop while another taker is still racing.
        let pool = setup_pool().await;
        let order_id = Uuid::new_v4();
        insert_order(&pool, order_id).await;
        sqlx::query("UPDATE orders SET status = ? WHERE id = ?")
            .bind(Status::WaitingTakerBond.to_string())
            .bind(order_id)
            .execute(&pool)
            .await
            .unwrap();

        let mut taker = make_bond(order_id, BondState::Requested);
        taker.pubkey = "t".repeat(64);
        taker.hash = Some("e".repeat(64));
        create_bond(&pool, taker).await.unwrap();

        let dropped = drop_waiting_taker_bond_to_pending(&pool, order_id)
            .await
            .unwrap();
        assert!(!dropped, "an active taker bond must keep the order parked");

        let status: String = sqlx::query_scalar("SELECT status FROM orders WHERE id = ?")
            .bind(order_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(status, Status::WaitingTakerBond.to_string());
    }

    #[tokio::test]
    async fn concurrent_requested_bonds_coexist() {
        // Multiple Requested bonds on the same order coexist — they
        // are not released at retake-time (that was the Phase 1
        // supersede behaviour, now removed). Cancellation of the
        // losers happens at lock-time via on_bond_invoice_accepted.
        let pool = setup_pool().await;
        let order_id = Uuid::new_v4();
        insert_order(&pool, order_id).await;

        for tag in ['a', 'b', 'c'] {
            let mut bond = make_bond(order_id, BondState::Requested);
            bond.pubkey = tag.to_string().repeat(64);
            bond.hash = Some(tag.to_string().repeat(64));
            create_bond(&pool, bond).await.unwrap();
        }

        let active = find_active_bonds_for_order(&pool, order_id).await.unwrap();
        assert_eq!(active.len(), 3);
        assert!(active
            .iter()
            .all(|b| b.state == BondState::Requested.to_string()));
    }

    #[tokio::test]
    async fn maybe_drop_waiting_taker_bond_noop_when_other_bonds_active() {
        // Phase 1.5: dropping the order back to Pending only fires when
        // *no* other active bond remains. With one bond still Requested,
        // the helper must short-circuit before touching the order (so
        // even without Nostr globals it never errors).
        let pool = setup_pool().await;
        let order_id = Uuid::new_v4();
        insert_order(&pool, order_id).await;
        // Mark order as WaitingTakerBond.
        sqlx::query("UPDATE orders SET status = ? WHERE id = ?")
            .bind(Status::WaitingTakerBond.to_string())
            .bind(order_id)
            .execute(&pool)
            .await
            .unwrap();
        let mut bond = make_bond(order_id, BondState::Requested);
        bond.hash = None;
        create_bond(&pool, bond).await.unwrap();

        maybe_drop_waiting_taker_bond(&pool, order_id)
            .await
            .expect("noop when other bonds active");

        // Status must NOT have flipped — Pending would imply the helper
        // ran the transition path despite the surviving bond.
        let order = Order::by_id(&pool, order_id).await.unwrap().unwrap();
        assert_eq!(order.status, Status::WaitingTakerBond.to_string());
    }

    #[tokio::test]
    async fn maybe_drop_waiting_taker_bond_noop_when_order_not_in_waiting_status() {
        // If the order is somehow not in `WaitingTakerBond` (e.g. Phase 1
        // legacy state, or a parallel path already flipped it), the
        // helper must no-op rather than blindly republish.
        let pool = setup_pool().await;
        let order_id = Uuid::new_v4();
        insert_order(&pool, order_id).await; // inserted as 'pending' by default

        maybe_drop_waiting_taker_bond(&pool, order_id)
            .await
            .expect("noop on non-WaitingTakerBond order");

        let order = Order::by_id(&pool, order_id).await.unwrap().unwrap();
        assert_eq!(order.status, Status::Pending.to_string());
    }

    /// Phase 1.5 P2 regression: `maybe_drop_waiting_taker_bond` must
    /// not flip a `WaitingTakerBond` order back to `Pending` if a
    /// concurrent bond has just become `Locked`. The single conditional
    /// UPDATE checks both predicates (status + no active bonds) in one
    /// statement, so the race window is closed at the SQL layer.
    #[tokio::test]
    async fn maybe_drop_does_not_revert_concurrent_lock() {
        let pool = setup_pool().await;
        let order_id = Uuid::new_v4();
        insert_order(&pool, order_id).await;
        sqlx::query("UPDATE orders SET status = ? WHERE id = ?")
            .bind(Status::WaitingTakerBond.to_string())
            .bind(order_id)
            .execute(&pool)
            .await
            .unwrap();
        // A `Locked` bond is the "concurrent winner racing past us"
        // scenario.
        let mut locked = make_bond(order_id, BondState::Locked);
        locked.hash = None;
        create_bond(&pool, locked).await.unwrap();

        maybe_drop_waiting_taker_bond(&pool, order_id)
            .await
            .expect("noop in the presence of a Locked bond");

        let order = Order::by_id(&pool, order_id).await.unwrap().unwrap();
        assert_eq!(
            order.status,
            Status::WaitingTakerBond.to_string(),
            "the CAS must NOT flip back to Pending while a Locked bond races"
        );
    }

    /// Phase 1.5 P1 regression: the `Pending → WaitingTakerBond` CAS
    /// in `request_taker_bond` must only flip when the live row is
    /// still `Pending`. If the bond subscriber wins the race (taker
    /// pays instantly) and transitions the order to `WaitingPayment`
    /// before our CAS runs, the CAS must refuse to overwrite. We can't
    /// invoke `request_taker_bond` directly in a unit test (LND), but
    /// we can exercise the exact CAS SQL it issues.
    #[tokio::test]
    async fn pending_to_waiting_taker_bond_cas_refuses_to_overwrite_concurrent_transition() {
        let pool = setup_pool().await;
        let order_id = Uuid::new_v4();
        insert_order(&pool, order_id).await;

        // Simulate a concurrent transition (e.g. `on_bond_invoice_accepted`
        // racing past us): the order is no longer `Pending`.
        sqlx::query("UPDATE orders SET status = ? WHERE id = ?")
            .bind(Status::WaitingPayment.to_string())
            .bind(order_id)
            .execute(&pool)
            .await
            .unwrap();

        // The CAS that `request_taker_bond` issues:
        let result = sqlx::query("UPDATE orders SET status = ? WHERE id = ? AND status = ?")
            .bind(Status::WaitingTakerBond.to_string())
            .bind(order_id)
            .bind(Status::Pending.to_string())
            .execute(&pool)
            .await
            .unwrap();

        assert_eq!(
            result.rows_affected(),
            0,
            "CAS must refuse to flip a no-longer-Pending order"
        );

        let order = Order::by_id(&pool, order_id).await.unwrap().unwrap();
        assert_eq!(
            order.status,
            Status::WaitingPayment.to_string(),
            "the concurrent transition must NOT be reverted"
        );
    }

    /// Companion to the CAS test: when the row is still `Pending`,
    /// the same SQL flips it cleanly.
    #[tokio::test]
    async fn pending_to_waiting_taker_bond_cas_flips_when_status_unchanged() {
        let pool = setup_pool().await;
        let order_id = Uuid::new_v4();
        insert_order(&pool, order_id).await; // inserted as 'pending'

        let result = sqlx::query("UPDATE orders SET status = ? WHERE id = ? AND status = ?")
            .bind(Status::WaitingTakerBond.to_string())
            .bind(order_id)
            .bind(Status::Pending.to_string())
            .execute(&pool)
            .await
            .unwrap();

        assert_eq!(result.rows_affected(), 1);
        let order = Order::by_id(&pool, order_id).await.unwrap().unwrap();
        assert_eq!(order.status, Status::WaitingTakerBond.to_string());
    }

    #[tokio::test]
    async fn maybe_drop_waiting_taker_bond_noop_when_order_missing() {
        // If the order row vanished between callsite and lookup, the
        // helper must no-op rather than propagate a hard error — the
        // bond is already marked Released by the time we get here, so
        // failing this best-effort cleanup would corrupt the call site's
        // error semantics.
        let pool = setup_pool().await;
        let order_id = Uuid::new_v4();
        // No INSERT — the order does not exist.

        maybe_drop_waiting_taker_bond(&pool, order_id)
            .await
            .expect("noop on missing order");
    }

    fn ln_err(msg: &str) -> MostroError {
        MostroInternalErr(ServiceError::LnNodeError(msg.to_string()))
    }

    #[test]
    fn classify_already_done_by_grpc_code() {
        // The `code=NotFound` / `code=AlreadyExists` prefix is what the
        // updated `cancel_hold_invoice` emits for benign outcomes.
        assert_eq!(
            classify_cancel_error(&ln_err("code=NotFound message=...")),
            CancelOutcome::AlreadyDone
        );
        assert_eq!(
            classify_cancel_error(&ln_err("code=AlreadyExists message=duplicate")),
            CancelOutcome::AlreadyDone
        );
    }

    #[test]
    fn classify_already_done_by_lnd_message() {
        // LND returns these under `code=Unknown`, so message inspection
        // is load-bearing.
        for msg in [
            "code=Unknown message=invoice with that hash already cancelled",
            "code=Unknown message=invoice with that hash already canceled",
            "code=Unknown message=unable to locate invoice",
            "code=Unknown message=invoice not found for hash",
            "code=Unknown message=no such invoice",
        ] {
            assert_eq!(
                classify_cancel_error(&ln_err(msg)),
                CancelOutcome::AlreadyDone,
                "expected AlreadyDone for: {msg}"
            );
        }
    }

    #[test]
    fn classify_transient_for_transport_and_unknown() {
        // Transport / server errors must NOT be treated as already-done:
        // marking Released here would strand the HTLC.
        for msg in [
            "code=Unavailable message=connection refused",
            "code=DeadlineExceeded message=timeout",
            "code=Internal message=server crashed",
            "code=Unknown message=something we don't recognise",
            "transport error",
        ] {
            assert_eq!(
                classify_cancel_error(&ln_err(msg)),
                CancelOutcome::Transient,
                "expected Transient for: {msg}"
            );
        }
    }
}
