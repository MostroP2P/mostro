//! Bond lifecycle wiring (Phase 1).
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
//! Protocol note: `mostro-core` 0.10.0 does not yet expose
//! `Action::AddBondInvoice` / `Status::WaitingTakerBond`. Phase 1 takes the
//! "Alternative" path documented in §6.2 of `docs/ANTI_ABUSE_BOND.md`:
//! orders stay in `Status::Pending` while waiting for the bond, and the
//! bond bolt11 ships to the taker as a regular `Action::PayInvoice` (the
//! semantics — "pay this Lightning invoice" — are an exact match). Bond
//! state lives entirely in the `bonds` table; clients identify the
//! invoice as a bond by its hash, which differs from the trade hold
//! invoice that follows once the bond is locked. The dedicated action /
//! status will land alongside the corresponding `mostro-core` release in a
//! later phase.

use std::str::FromStr;
use std::sync::Arc;

use chrono::Utc;
use fedimint_tonic_lnd::lnrpc::invoice::InvoiceState;
use mostro_core::error::{MostroError, MostroError::MostroInternalErr, ServiceError};
use mostro_core::prelude::*;
use nostr_sdk::nostr::hashes::hex::FromHex;
use nostr_sdk::prelude::*;
use sqlx::{Pool, Sqlite};
use sqlx_crud::Crud;
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

/// Create a hold invoice for the taker's bond, persist a `Bond` row in
/// `Requested`, ship the bolt11 to the taker, and start the LND
/// subscriber that flips the row to `Locked` once the taker pays.
///
/// On any failure inside this function the bond row may exist in
/// `Requested` with no LND counterpart — that's fine: Phase 1's
/// "always release" guarantee covers it on the next exit.
pub async fn request_taker_bond(
    pool: &Pool<Sqlite>,
    order: &Order,
    taker_pubkey: PublicKey,
    request_id: Option<u64>,
    trade_index: Option<i64>,
) -> Result<Bond, MostroError> {
    let cfg = Settings::get_bond().ok_or_else(|| {
        MostroInternalErr(ServiceError::UnexpectedError(
            "anti_abuse_bond block is missing while bond was deemed required".into(),
        ))
    })?;

    let amount = compute_bond_amount(order.amount, cfg);
    let memo = format!("Bond for Mostro order {}", order.id);

    let mut ln_client = LndConnector::new().await?;
    let (invoice_resp, preimage, hash) = ln_client
        .create_hold_invoice(&memo, amount)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::HoldInvoiceError(e.to_string())))?;

    let mut bond = Bond::new_requested(order.id, taker_pubkey.to_string(), BondRole::Taker, amount);
    bond.hash = Some(bytes_to_string(&hash));
    bond.preimage = Some(bytes_to_string(&preimage));
    bond.payment_request = Some(invoice_resp.payment_request.clone());

    let bond = create_bond(pool, bond).await?;

    info!(
        "Bond requested: bond_id={} order_id={} role={} amount_sats={}",
        bond.id, order.id, bond.role, bond.amount_sats
    );

    // Phase-1 alternative path (see module-level doc): the bond bolt11
    // ships as a regular `PayInvoice`. The `SmallOrder` echoes the order
    // id so a bond-aware client can correlate — and a non-bond-aware
    // client just sees an extra invoice to pay before the trade.
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

    enqueue_order_msg(
        request_id,
        Some(order.id),
        Action::PayInvoice,
        Some(Payload::PaymentRequest(
            Some(bond_small),
            invoice_resp.payment_request,
            None,
        )),
        taker_pubkey,
        trade_index,
    )
    .await;

    bond_invoice_subscribe(hash, request_id).await?;

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
/// — the [issue raised in the Phase 1 review](#).
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
    let bonds = find_active_bonds_for_order(pool, order_id).await?;
    for bond in bonds.iter() {
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

/// Subscriber callback for `InvoiceState::Accepted`: bond is locked.
///
/// Drives the bond from `Requested` to `Locked` via a conditional
/// `UPDATE`, then — independently of whether *this* call won the
/// transition — attempts to resume the take flow if (a) the bond is
/// `Locked` and (b) the order is still `Pending`.
///
/// Decoupling the bond-state transition from the resume retry means a
/// transient resume failure (LND/DB/Nostr blip while creating the
/// trade hold invoice) doesn't leave the order stuck: the next
/// `Accepted` delivery — or the restart resubscriber — will retry the
/// continuation as long as both conditions still hold. Conversely,
/// if the order has moved out of `Pending` (resume already succeeded,
/// or maker/admin canceled in the meantime) the resume is skipped, so
/// we never reactivate a canceled order.
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

    // Atomic Requested → Locked transition. Concurrent firings — LND
    // reconnect, the restart-time resubscriber, etc. — race here and
    // exactly one wins; the others see `rows_affected == 0` and fall
    // through to the post-transition retry logic below.
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

    if result.rows_affected() == 1 {
        info!("Bond {} locked for order {}", bond.id, bond.order_id);
    }

    // Re-read the bond so a concurrent release (Locked → Released) is
    // visible: in that case there's nothing to resume.
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
    if current_state != BondState::Locked {
        // Released / Slashed / Failed / Requested-still-but-something-
        // else-is-wrong: nothing to resume on this firing.
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
    // still in the pre-trade state we left it in. If it's already moved
    // on (resume succeeded on a previous firing) or been canceled by a
    // maker / admin / scheduler path, do not re-trigger the take.
    if order.status != Status::Pending.to_string() {
        info!(
            "Bond {} accepted but order {} is in status {} — skipping resume",
            current.id, order.id, order.status
        );
        return Ok(());
    }

    let my_keys = get_keys()?;
    resume_take_after_bond(pool, order, &my_keys, request_id).await
}

/// Subscriber callback for `InvoiceState::Canceled`: bond never locked
/// (taker abandoned the invoice, or LND auto-canceled on expiration).
///
/// Phase 1 keeps the order untouched: it stays `Pending` with the taker
/// fields populated. The maker's order remains discoverable via the
/// existing Nostr event. A follow-up phase (or operator action) can
/// reset the order if needed; for Phase 1, "always release" is the only
/// guarantee we owe.
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
    Ok(())
}

/// Resume the take flow after the bond locks.
///
/// The taker side already populated all trade fields on the order before
/// requesting the bond, so this function only needs to drive the trade
/// hold invoice / payout-invoice request that `take_*_action` deferred.
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
        sqlx::query(include_str!(
            "../../../migrations/20260423120000_anti_abuse_bond.sql"
        ))
        .execute(&pool)
        .await
        .expect("bonds migration");
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
