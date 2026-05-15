//! Phase 3 — bond payout flow.
//!
//! Shared infrastructure that drains the queue of bonds Phase 2 left in
//! [`BondState::PendingPayout`] (and Phase 4+/5+/7 will fill the same
//! queue from the timeout and maker paths). Two entry points:
//!
//! - [`run_bond_payout_cycle`] — called once per scheduler tick. Walks
//!   every `PendingPayout` bond and drives it forward by one step: ask
//!   the non-slashed counterparty for a bolt11 if we haven't yet, then
//!   `send_payment` for the counterparty share. Transitions to
//!   `Forfeited` when `payout_claim_window_days` has elapsed with no
//!   invoice.
//! - [`add_bond_invoice_action`] — the action handler that consumes the
//!   counterparty's `Action::AddBondInvoice` reply, validates it, and
//!   persists the bolt11 onto the bond row.
//!
//! ## Slashed HTLC already claimed
//!
//! The bond hold invoice is **settled at slash time** in
//! [`super::slash::apply_bond_resolution`], not here. By the time this
//! module sees a `PendingPayout` row, the sats are already in Mostro's
//! wallet — this scheduler only drives the counterparty payout
//! (request bolt11 → `send_payment` → retries / forfeiture). Phase 3
//! never calls `settle_hold_invoice`.
//!
//! ## Why a dedicated action type
//!
//! Phase 3 originally reused `Action::AddInvoice` (the buyer-flow
//! action) and disambiguated by looking up a `PendingPayout` bond for
//! the order/sender pair. The protocol decision in this PR is to ship
//! `Action::AddBondInvoice` instead — it's the counterparty-direction
//! dual of Phase 1.5's `Action::PayBondInvoice`, and keeps the two
//! invoice flows disjoint at the routing layer. See
//! `docs/ANTI_ABUSE_BOND.md` §8.1 and §14.3.
//!
//! ## Split snapshot
//!
//! Every `PendingPayout` row was written with `node_share_sats` frozen
//! at the moment of transition (Phase 2 §7.3 step 4). The scheduler
//! reads that column; it never re-reads `slash_node_share_pct` from
//! config. This makes the split deterministic across daemon restarts
//! and operator config changes.
//!
//! ## Recipient resolution
//!
//! The non-slashed counterparty is recomputed from `order.{buyer,
//! seller}_pubkey` + `bond.pubkey` + `slashed_reason` at scheduler time.
//! No new schema column is needed; the same mapping the Phase 2
//! validator uses on the way *in* applies here on the way *out*.

use std::str::FromStr;
use std::time::Duration;

use chrono::Utc;
use fedimint_tonic_lnd::lnrpc::payment::PaymentStatus;
use mostro_core::error::{
    CantDoReason,
    MostroError::{self, MostroCantDo, MostroInternalErr},
    ServiceError,
};
use mostro_core::message::{Action, Message, Payload};
use mostro_core::nip59::UnwrappedMessage;
use mostro_core::order::{Order, SmallOrder};
use nostr_sdk::prelude::*;
use sqlx::{Pool, Sqlite};
use sqlx_crud::Crud;
use tokio::sync::mpsc::channel;
use tokio::time::timeout;
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::app::context::AppContext;
use crate::config::settings::Settings;
use crate::lightning::invoice::is_valid_invoice;
use crate::lightning::LndConnector;
use crate::util::enqueue_order_msg;

use super::db::find_bonds_by_state;
use super::model::Bond;
use super::types::{BondSlashReason, BondState};

/// Per-message ceiling for the `send_payment` status stream. LND
/// streams periodic InFlight updates while a payment is routing; if no
/// update lands inside this window the channel is treated as dead and
/// the attempt is routed through `on_send_payment_failure`. Picked to
/// be longer than the typical InFlight cadence (a few seconds) but
/// short enough to keep a single bond from blocking a scheduler task
/// indefinitely.
const PAYMENT_STATUS_RECV_TIMEOUT: Duration = Duration::from_secs(120);

/// One full pass over every bond in [`BondState::PendingPayout`].
///
/// Mirror of `dev_fee::run_dev_fee_cycle`: each tick walks the work
/// queue and advances each row by at most one step. The scheduler
/// calls this from a single task, so there is no in-process contention
/// — but every state transition is still done with a guarded CAS so a
/// concurrent admin retry or a daemon restart that re-fires the same
/// tick cannot double-settle a row.
pub async fn run_bond_payout_cycle(pool: &Pool<Sqlite>, ln_client: &mut LndConnector) {
    let bonds = match find_bonds_by_state(pool, BondState::PendingPayout).await {
        Ok(b) => b,
        Err(e) => {
            error!("bond payout: failed to enumerate PendingPayout bonds: {e}");
            return;
        }
    };
    if bonds.is_empty() {
        return;
    }
    info!(
        "bond payout: processing {} PendingPayout bond(s)",
        bonds.len()
    );

    for bond in bonds {
        if let Err(e) = process_one_bond(pool, ln_client, &bond).await {
            warn!(
                bond_id = %bond.id,
                order_id = %bond.order_id,
                "bond payout: bond cycle errored: {e}"
            );
        }
    }
}

/// Drive a single bond forward by one step.
///
/// The bond HTLC was already settled at slash time (Phase 2 §7.3); this
/// state machine only governs the counterparty payout leg:
///
/// ```text
///                          ┌── forfeit window elapsed AND no invoice ──► Forfeited
///                          │
///   PendingPayout ─────────┤── no invoice yet AND cadence ok ──► AddBondInvoice message
///                          │
///                          │── invoice present (or node_share_pct=1.0) ──► [send_payment]
///                          │                                                      │
///                          └──── send_payment success ─► Slashed                  │
///                                                                                 │
///                                    send_payment failure ─► retry (or Failed) ◄──┘
/// ```
///
/// Each call advances by at most one of these arms; the scheduler
/// reruns the row on the next tick until a terminal state is reached.
async fn process_one_bond(
    pool: &Pool<Sqlite>,
    ln_client: &mut LndConnector,
    bond: &Bond,
) -> Result<(), MostroError> {
    let cfg = Settings::get_bond();
    let claim_window_seconds = cfg
        .map(|c| c.payout_claim_window_days as i64 * 86_400)
        .unwrap_or(15 * 86_400);
    let invoice_window_seconds = cfg
        .map(|c| c.payout_invoice_window_seconds as i64)
        .unwrap_or(300);
    let max_retries = cfg.map(|c| c.payout_max_retries as i64).unwrap_or(5);

    let now = Utc::now().timestamp();
    // Phase 2's slash CAS writes `slashed_at` atomically with the
    // transition to `PendingPayout`. A `None` here is an invariant
    // violation; defaulting to `now` would silently reset the forfeit
    // anchor and could indefinitely defer the forfeit transition.
    let slashed_at = bond.slashed_at.ok_or_else(|| {
        MostroInternalErr(ServiceError::UnexpectedError(format!(
            "bond {} in PendingPayout missing slashed_at (invariant violation)",
            bond.id
        )))
    })?;

    // Forfeit window has elapsed and counterparty never submitted a
    // bolt11: transition to `Forfeited`. The HTLC was already settled
    // at slash time, so the sats are already in Mostro's wallet — no
    // further LND interaction, no `send_payment` attempts. The node
    // retains `amount_sats` in full.
    if bond.payout_invoice.is_none() && now - slashed_at >= claim_window_seconds {
        return forfeit_bond(pool, bond).await;
    }

    // Normal payout: either request an invoice (counterparty leg), or
    // pay the counterparty from the already-settled HTLC funds.
    let counterparty_share = counterparty_share_sats(bond)?;

    if counterparty_share <= 0 {
        // `slash_node_share_pct = 1.0` (or full retention) — there's no
        // counterparty leg. Transition straight to `Slashed`. No
        // `AddBondInvoice` message, no `send_payment`.
        return finalize_node_only(pool, bond).await;
    }

    match bond.payout_invoice.as_deref() {
        None => request_payout_invoice(pool, bond, invoice_window_seconds).await,
        Some(invoice) => pay_counterparty(pool, ln_client, bond, invoice, max_retries).await,
    }
}

/// Compute `amount_sats - node_share_sats`, erroring if the row is
/// missing `node_share_sats` or carries an out-of-range value. Phase 2's
/// slash CAS writes the column atomically with the transition to
/// `PendingPayout`, so a `None` here is an invariant violation that
/// should not be defaulted to 0 — silently treating node share as 0
/// would pay the entire bond to the counterparty. The same goes for
/// values outside `0..=amount_sats`: a negative share would corrupt the
/// invoice principal, and a share above the bond would imply Mostro
/// retains more than the user locked. The config's
/// `slash_node_share_pct` validator already rejects out-of-range
/// fractions at startup, but we re-check here as a belt-and-braces
/// guard against a corrupted DB row.
fn counterparty_share_sats(bond: &Bond) -> Result<i64, MostroError> {
    let node_share = bond.node_share_sats.ok_or_else(|| {
        MostroInternalErr(ServiceError::UnexpectedError(format!(
            "bond {} in PendingPayout missing node_share_sats (invariant violation)",
            bond.id
        )))
    })?;
    if !(0..=bond.amount_sats).contains(&node_share) {
        return Err(MostroInternalErr(ServiceError::UnexpectedError(format!(
            "bond {} in PendingPayout has node_share_sats {node_share} outside [0, {}] (invariant violation)",
            bond.id, bond.amount_sats
        ))));
    }
    Ok(bond.amount_sats - node_share)
}

/// Transition a `PendingPayout` row to `Forfeited`.
///
/// The HTLC was settled at slash time, so the sats are already in
/// Mostro's wallet; this function only flips the state. The
/// `AND payout_invoice IS NULL` predicate closes the race against
/// `add_bond_invoice_action`: if the counterparty's late invoice
/// landed between this scheduler's `bond.payout_invoice.is_none()`
/// snapshot and the UPDATE below, we must not flip the row to
/// `Forfeited` and silently discard their bolt11 — on the next tick
/// `pay_counterparty` will take over and route the funds.
async fn forfeit_bond(pool: &Pool<Sqlite>, bond: &Bond) -> Result<(), MostroError> {
    let result = sqlx::query(
        "UPDATE bonds SET state = ? WHERE id = ? AND state = ? AND payout_invoice IS NULL",
    )
    .bind(BondState::Forfeited.to_string())
    .bind(bond.id)
    .bind(BondState::PendingPayout.to_string())
    .execute(pool)
    .await
    .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

    if result.rows_affected() == 1 {
        info!(
            bond_id = %bond.id,
            order_id = %bond.order_id,
            amount_sats = bond.amount_sats,
            "bond forfeited: claim window elapsed, node retains full amount"
        );
    } else {
        info!(
            bond_id = %bond.id,
            order_id = %bond.order_id,
            "forfeit: counterparty invoice landed concurrently; payout will continue on next tick"
        );
    }
    Ok(())
}

/// Counterparty share is empty (`slash_node_share_pct = 1.0`): the
/// HTLC was already settled at slash time, so there is nothing left
/// for the scheduler to do beyond flipping the row to `Slashed`. No
/// messages, no `send_payment`.
async fn finalize_node_only(pool: &Pool<Sqlite>, bond: &Bond) -> Result<(), MostroError> {
    let result = sqlx::query("UPDATE bonds SET state = ? WHERE id = ? AND state = ?")
        .bind(BondState::Slashed.to_string())
        .bind(bond.id)
        .bind(BondState::PendingPayout.to_string())
        .execute(pool)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

    if result.rows_affected() == 1 {
        info!(
            bond_id = %bond.id,
            order_id = %bond.order_id,
            amount_sats = bond.amount_sats,
            "bond slashed (node-only): full amount retained by Mostro"
        );
    }
    Ok(())
}

/// Ask the non-slashed counterparty for a payout bolt11 via
/// `Action::AddBondInvoice`. Skip if the previous request landed
/// within `payout_invoice_window_seconds`. Bumps
/// `invoice_request_attempts` and `last_invoice_request_at` only — the
/// retry budget (`payout_max_retries`) is for `send_payment` failures
/// once an invoice is in hand, not for invoice-request messages.
async fn request_payout_invoice(
    pool: &Pool<Sqlite>,
    bond: &Bond,
    invoice_window_seconds: i64,
) -> Result<(), MostroError> {
    let now = Utc::now().timestamp();
    if let Some(last) = bond.last_invoice_request_at {
        if now - last < invoice_window_seconds {
            return Ok(());
        }
    }

    let order = match Order::by_id(pool, bond.order_id)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?
    {
        Some(o) => o,
        None => {
            warn!(
                bond_id = %bond.id,
                order_id = %bond.order_id,
                "request_payout_invoice: order row missing; skipping"
            );
            return Ok(());
        }
    };

    let reason = bond
        .slashed_reason
        .as_deref()
        .and_then(|s| BondSlashReason::from_str(s).ok())
        .ok_or_else(|| {
            MostroInternalErr(ServiceError::UnexpectedError(format!(
                "bond {} in PendingPayout has unparseable slashed_reason {:?}",
                bond.id, bond.slashed_reason
            )))
        })?;

    let recipient_pubkey = match resolve_recipient(&order, bond, reason)? {
        Some(pk) => pk,
        None => {
            warn!(
                bond_id = %bond.id,
                order_id = %bond.order_id,
                "request_payout_invoice: cannot resolve recipient; skipping (will retry on next tick)"
            );
            return Ok(());
        }
    };

    let counterparty_share = counterparty_share_sats(bond)?;
    let order_kind = order.get_order_kind().map_err(MostroInternalErr)?;

    let small = SmallOrder::new(
        Some(order.id),
        Some(order_kind),
        None,
        counterparty_share,
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

    info!(
        bond_id = %bond.id,
        order_id = %bond.order_id,
        amount_sats = counterparty_share,
        recipient = %recipient_pubkey,
        attempt = bond.invoice_request_attempts + 1,
        "bond payout: requesting invoice from counterparty"
    );

    // The notification carries only the structured request payload. The
    // client computes the forfeit deadline from `slashed_at` (the slash
    // moment is observable on-chain via the audit event the dispute /
    // timeout path publishes) plus `bond_payout_claim_window_days` from
    // the kind-38385 info event, and presents it to the user in their own
    // locale. Mostro intentionally ships no hardcoded human-readable
    // text.
    enqueue_order_msg(
        None,
        Some(order.id),
        Action::AddBondInvoice,
        Some(Payload::Order(small)),
        recipient_pubkey,
        None,
    )
    .await;

    sqlx::query(
        "UPDATE bonds \
           SET invoice_request_attempts = invoice_request_attempts + 1, \
               last_invoice_request_at = ? \
         WHERE id = ? AND state = ?",
    )
    .bind(now)
    .bind(bond.id)
    .bind(BondState::PendingPayout.to_string())
    .execute(pool)
    .await
    .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

    Ok(())
}

/// `send_payment` the counterparty share to the bolt11 they
/// submitted. The bond HTLC was already settled at slash time, so the
/// sats are already in Mostro's wallet — this function only drives
/// the counterparty leg and on success transitions the row to
/// `Slashed`.
async fn pay_counterparty(
    pool: &Pool<Sqlite>,
    ln_client: &mut LndConnector,
    bond: &Bond,
    invoice: &str,
    max_retries: i64,
) -> Result<(), MostroError> {
    let counterparty_share = counterparty_share_sats(bond)?;

    // Routing-fee ceiling: derived from MostroSettings::max_routing_fee
    // applied to the counterparty share. The spec mentions
    // `query_routes` as a finer estimate, but `send_payment` already
    // accepts a `fee_limit_sat` and the existing helper computes that.
    // We record what we used into `payout_routing_fee_sats` so
    // operators can read the cap from logs.
    let max_routing_fee = Settings::get_mostro().max_routing_fee;
    let routing_fee_cap = ((counterparty_share as f64) * max_routing_fee).ceil() as i64;
    let _ = sqlx::query("UPDATE bonds SET payout_routing_fee_sats = ? WHERE id = ? AND state = ?")
        .bind(routing_fee_cap)
        .bind(bond.id)
        .bind(BondState::PendingPayout.to_string())
        .execute(pool)
        .await;

    // send_payment. The helper internally caps the fee at
    // `counterparty_share * max_routing_fee`.
    let (tx, mut rx) = channel(100);
    let send_outcome = ln_client
        .send_payment(invoice, counterparty_share, tx)
        .await;
    if let Err(e) = send_outcome {
        return on_send_payment_failure(pool, bond, max_retries, &format!("{e}")).await;
    }

    // Collect the first terminal status from the stream. Mirrors
    // dev_fee::send_dev_fee_payment, but each recv is bounded by
    // `PAYMENT_STATUS_RECV_TIMEOUT` so a wedged LND stream (no terminal
    // update, no EOF, no InFlight churn) does not pin the scheduler
    // task forever. A timeout and a clean EOF are both routed through
    // `on_send_payment_failure` so the retry budget governs the
    // recovery path uniformly.
    let mut succeeded = false;
    let mut failure: Option<String> = None;
    loop {
        match timeout(PAYMENT_STATUS_RECV_TIMEOUT, rx.recv()).await {
            Err(_) => {
                failure = Some(format!(
                    "payment status stream timed out after {}s without a terminal update",
                    PAYMENT_STATUS_RECV_TIMEOUT.as_secs()
                ));
                break;
            }
            Ok(None) => break,
            Ok(Some(msg)) => {
                if let Ok(status) = PaymentStatus::try_from(msg.payment.status) {
                    match status {
                        PaymentStatus::Succeeded => {
                            succeeded = true;
                            break;
                        }
                        PaymentStatus::Failed => {
                            failure = Some(format!(
                                "payment failed: reason {}",
                                msg.payment.failure_reason
                            ));
                            break;
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    if succeeded {
        let result = sqlx::query("UPDATE bonds SET state = ? WHERE id = ? AND state = ?")
            .bind(BondState::Slashed.to_string())
            .bind(bond.id)
            .bind(BondState::PendingPayout.to_string())
            .execute(pool)
            .await
            .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
        if result.rows_affected() == 1 {
            info!(
                bond_id = %bond.id,
                order_id = %bond.order_id,
                amount_sats = bond.amount_sats,
                counterparty_share_sats = counterparty_share,
                "bond payout: send_payment succeeded; bond transitioned to Slashed"
            );
        }
        return Ok(());
    }

    let msg = failure.unwrap_or_else(|| "payment stream ended without terminal status".to_string());
    on_send_payment_failure(pool, bond, max_retries, &msg).await
}

/// Bump `payout_attempts`; on `payout_max_retries` reached, transition
/// the bond to `Failed`. This counter only increments on real
/// `send_payment` failures, not on invoice-request messages.
async fn on_send_payment_failure(
    pool: &Pool<Sqlite>,
    bond: &Bond,
    max_retries: i64,
    cause: &str,
) -> Result<(), MostroError> {
    let new_attempts = bond.payout_attempts + 1;
    sqlx::query("UPDATE bonds SET payout_attempts = ? WHERE id = ? AND state = ?")
        .bind(new_attempts)
        .bind(bond.id)
        .bind(BondState::PendingPayout.to_string())
        .execute(pool)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

    if new_attempts >= max_retries {
        sqlx::query("UPDATE bonds SET state = ? WHERE id = ? AND state = ?")
            .bind(BondState::Failed.to_string())
            .bind(bond.id)
            .bind(BondState::PendingPayout.to_string())
            .execute(pool)
            .await
            .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
        error!(
            bond_id = %bond.id,
            order_id = %bond.order_id,
            attempts = new_attempts,
            "bond payout: send_payment exhausted retries — transitioning to Failed; node share retained, counterparty share stranded (operator review required). last error: {cause}"
        );
    } else {
        warn!(
            bond_id = %bond.id,
            order_id = %bond.order_id,
            attempts = new_attempts,
            max_retries,
            "bond payout: send_payment failure ({cause}); will retry on next tick"
        );
    }
    Ok(())
}

/// Map `(order, bond, reason)` to the non-slashed counterparty's
/// pubkey. Returns `None` when the order's buyer/seller fields are
/// unset (e.g. an in-flight take that never completed) so the caller
/// can no-op and retry on the next tick rather than crash.
///
/// - **`LostDispute`.** The bonded user is the slashed side; the
///   recipient is whoever is *not* `bond.pubkey` on the order. This
///   collapses the four §3.1 cases (sell/buy × seller/buyer) into a
///   single lookup at this layer.
/// - **`Timeout` (Phase 4+).** Same logic — the slashed party is the
///   one responsible for the elapsed waiting state, and the recipient
///   is the other side. The §9.2 table is encoded by *who got
///   slashed*, not consulted here.
fn resolve_recipient(
    order: &Order,
    bond: &Bond,
    _reason: BondSlashReason,
) -> Result<Option<PublicKey>, MostroError> {
    let buyer = order.buyer_pubkey.as_deref();
    let seller = order.seller_pubkey.as_deref();
    let recipient_str = match (buyer, seller) {
        (Some(b), Some(s)) if bond.pubkey == b => Some(s),
        (Some(b), Some(s)) if bond.pubkey == s => Some(b),
        _ => None,
    };
    let pk = recipient_str
        .map(PublicKey::from_str)
        .transpose()
        .map_err(|e| MostroInternalErr(ServiceError::UnexpectedError(e.to_string())))?;
    Ok(pk)
}

// ── Inbound action handler ──────────────────────────────────────────────

/// `Action::AddBondInvoice` handler: the counterparty replies with the
/// payout bolt11. Persists `payout_invoice` and resets
/// `invoice_request_attempts` to 0 in the same UPDATE.
///
/// Rejects with `CantDo(NotAllowedByStatus)` if the bond has already
/// moved out of `PendingPayout` — most importantly the `Forfeited`
/// case, which is the "claim window expired" path. The CAS predicate
/// is the arbiter; no clocks are read here.
pub async fn add_bond_invoice_action(
    ctx: &AppContext,
    msg: Message,
    event: &UnwrappedMessage,
    _my_keys: &Keys,
) -> Result<(), MostroError> {
    let pool = ctx.pool();
    let kind = msg.get_inner_message_kind();
    let order_id = kind.id.ok_or(MostroCantDo(CantDoReason::InvalidPayload))?;
    let payment_request = match kind.get_payment_request() {
        Some(pr) if !pr.is_empty() => pr,
        _ => return Err(MostroCantDo(CantDoReason::InvalidInvoice)),
    };

    let sender = event.sender;
    let bond = find_active_bond_in_pending_payout(pool, order_id, &sender.to_string()).await?;
    let bond = match bond {
        Some(b) => b,
        None => {
            // No `PendingPayout` bond for this sender on this order.
            // Two natural reasons: the bond was never slashed (and so
            // there is nothing to invoice for), or the claim window
            // already expired and the row moved to `Forfeited`. From
            // the user's perspective both look the same — they sent
            // us an invoice we cannot route.
            return Err(MostroCantDo(CantDoReason::NotAllowedByStatus));
        }
    };

    let counterparty_share = counterparty_share_sats(&bond)?;
    if counterparty_share <= 0 {
        // `slash_node_share_pct = 1.0` — there is no counterparty
        // share to pay out. Reject so the user isn't left waiting on
        // a payment that will never come.
        return Err(MostroCantDo(CantDoReason::NotAllowedByStatus));
    }

    // Validate the bolt11 amount matches the counterparty share. Fee
    // 0 because the counterparty share is what arrives at the
    // recipient; routing fees come out of Mostro's own wallet, not
    // the invoice principal.
    if is_valid_invoice(
        payment_request.clone(),
        Some(counterparty_share as u64),
        Some(0),
    )
    .await
    .is_err()
    {
        return Err(MostroCantDo(CantDoReason::InvalidInvoice));
    }

    // CAS the row: only persist the invoice if the bond is *still* in
    // `PendingPayout` and *still* has no `payout_invoice`. Both
    // predicates close the obvious races (the scheduler racing to
    // Forfeited; a concurrent duplicate AddBondInvoice).
    let result = sqlx::query(
        "UPDATE bonds \
           SET payout_invoice = ?, invoice_request_attempts = 0 \
         WHERE id = ? AND state = ? AND payout_invoice IS NULL",
    )
    .bind(&payment_request)
    .bind(bond.id)
    .bind(BondState::PendingPayout.to_string())
    .execute(pool)
    .await
    .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

    if result.rows_affected() == 0 {
        return Err(MostroCantDo(CantDoReason::NotAllowedByStatus));
    }

    info!(
        bond_id = %bond.id,
        order_id = %bond.order_id,
        sender = %sender,
        "bond payout: invoice accepted; awaiting scheduler tick for settle+pay"
    );

    Ok(())
}

/// Find a `PendingPayout` bond on `order_id` whose recipient (the
/// non-slashed side) matches `sender_pubkey`.
///
/// The bond row stores `bond.pubkey` = slashed user's trade pubkey,
/// not the recipient's. So we look up the order, compute the
/// recipient via [`resolve_recipient`], and confirm it matches the
/// sender of the AddBondInvoice reply.
async fn find_active_bond_in_pending_payout(
    pool: &Pool<Sqlite>,
    order_id: Uuid,
    sender_pubkey: &str,
) -> Result<Option<Bond>, MostroError> {
    let bonds: Vec<Bond> = sqlx::query_as::<_, Bond>(
        "SELECT * FROM bonds WHERE order_id = ? AND state = ? \
         ORDER BY slashed_at DESC",
    )
    .bind(order_id)
    .bind(BondState::PendingPayout.to_string())
    .fetch_all(pool)
    .await
    .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

    if bonds.is_empty() {
        return Ok(None);
    }

    let order = match Order::by_id(pool, order_id)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?
    {
        Some(o) => o,
        None => return Ok(None),
    };

    for bond in bonds {
        let reason = match bond
            .slashed_reason
            .as_deref()
            .and_then(|r| BondSlashReason::from_str(r).ok())
        {
            Some(r) => r,
            None => continue,
        };
        if let Some(recipient) = resolve_recipient(&order, &bond, reason)? {
            if recipient.to_string() == sender_pubkey {
                return Ok(Some(bond));
            }
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::bond::db::create_bond;
    use crate::app::bond::types::BondRole;
    use mostro_core::order::{Kind, Status};
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
        pool
    }

    fn taker_pk() -> &'static str {
        "1111111111111111111111111111111111111111111111111111111111111111"
    }
    fn maker_pk() -> &'static str {
        "2222222222222222222222222222222222222222222222222222222222222222"
    }

    async fn insert_order(pool: &Pool<Sqlite>, id: Uuid, seller: &str, buyer: &str) {
        sqlx::query(
            r#"INSERT INTO orders (
                id, kind, event_id, status, premium, payment_method,
                amount, fiat_code, fiat_amount, created_at, expires_at,
                seller_pubkey, buyer_pubkey
            ) VALUES (?, 'sell', ?, ?, 0, 'cash', 100000, 'USD', 10, 0, 0, ?, ?)"#,
        )
        .bind(id)
        .bind(id.simple().to_string())
        .bind(Status::Dispute.to_string())
        .bind(seller)
        .bind(buyer)
        .execute(pool)
        .await
        .expect("insert order");
    }

    fn pending_payout_bond(
        order_id: Uuid,
        pubkey: &str,
        amount: i64,
        node_share: i64,
        slashed_at: i64,
        invoice: Option<&str>,
        last_request: Option<i64>,
    ) -> Bond {
        let mut b = Bond::new_requested(order_id, pubkey.to_string(), BondRole::Taker, amount);
        b.state = BondState::PendingPayout.to_string();
        b.node_share_sats = Some(node_share);
        b.slashed_reason = Some(BondSlashReason::LostDispute.to_string());
        b.slashed_at = Some(slashed_at);
        b.payout_invoice = invoice.map(|s| s.to_string());
        b.last_invoice_request_at = last_request;
        b
    }

    #[test]
    fn resolve_recipient_sell_order_taker_buyer_slashed() {
        // Sell-order: maker=seller, taker=buyer. Bond is on the
        // taker (buyer). Recipient = seller (the non-slashed side).
        let order = Order {
            kind: Kind::Sell.to_string(),
            seller_pubkey: Some(maker_pk().to_string()),
            buyer_pubkey: Some(taker_pk().to_string()),
            ..Order::default()
        };
        let bond = pending_payout_bond(Uuid::new_v4(), taker_pk(), 10_000, 5_000, 0, None, None);
        let r = resolve_recipient(&order, &bond, BondSlashReason::LostDispute).unwrap();
        assert_eq!(r.unwrap().to_string(), maker_pk());
    }

    #[test]
    fn resolve_recipient_buy_order_taker_seller_slashed() {
        let order = Order {
            kind: Kind::Buy.to_string(),
            buyer_pubkey: Some(maker_pk().to_string()),
            seller_pubkey: Some(taker_pk().to_string()),
            ..Order::default()
        };
        let bond = pending_payout_bond(Uuid::new_v4(), taker_pk(), 10_000, 5_000, 0, None, None);
        let r = resolve_recipient(&order, &bond, BondSlashReason::LostDispute).unwrap();
        assert_eq!(r.unwrap().to_string(), maker_pk());
    }

    #[test]
    fn resolve_recipient_missing_buyer_returns_none() {
        let order = Order {
            kind: Kind::Sell.to_string(),
            seller_pubkey: Some(maker_pk().to_string()),
            buyer_pubkey: None,
            ..Order::default()
        };
        let bond = pending_payout_bond(Uuid::new_v4(), taker_pk(), 10_000, 5_000, 0, None, None);
        let r = resolve_recipient(&order, &bond, BondSlashReason::LostDispute).unwrap();
        assert!(r.is_none());
    }

    #[tokio::test]
    async fn request_payout_invoice_respects_cadence_window() {
        // A request issued within `invoice_window_seconds` of the
        // previous one must no-op. This is the load-bearing guard
        // against spamming the counterparty on every 60s tick.
        let pool = setup_pool().await;
        let order_id = Uuid::new_v4();
        insert_order(&pool, order_id, maker_pk(), taker_pk()).await;
        let now = Utc::now().timestamp();
        let bond = pending_payout_bond(
            order_id,
            taker_pk(),
            10_000,
            5_000,
            now,
            None,
            Some(now - 10), // 10s ago, well within 300s window
        );
        let bond = create_bond(&pool, bond).await.unwrap();

        request_payout_invoice(&pool, &bond, 300).await.unwrap();

        let row: (i64, Option<i64>) = sqlx::query_as(
            "SELECT invoice_request_attempts, last_invoice_request_at FROM bonds WHERE id = ?",
        )
        .bind(bond.id)
        .fetch_one(&pool)
        .await
        .unwrap();
        // Counter must NOT have advanced and timestamp must be
        // untouched.
        assert_eq!(row.0, 0);
        assert_eq!(row.1, Some(now - 10));
    }

    #[tokio::test]
    async fn forfeit_bond_transitions_pending_to_forfeited() {
        // The HTLC is settled at slash time (Phase 2), so `forfeit_bond`
        // is now a pure SQL transition with the `payout_invoice IS NULL`
        // CAS guard. No LND dependency.
        let pool = setup_pool().await;
        let order_id = Uuid::new_v4();
        insert_order(&pool, order_id, maker_pk(), taker_pk()).await;
        let now = Utc::now().timestamp();
        let bond = pending_payout_bond(order_id, taker_pk(), 10_000, 5_000, now, None, None);
        let bond = create_bond(&pool, bond).await.unwrap();

        forfeit_bond(&pool, &bond).await.unwrap();

        let after: (String,) = sqlx::query_as("SELECT state FROM bonds WHERE id = ?")
            .bind(bond.id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(after.0, BondState::Forfeited.to_string());
    }

    #[tokio::test]
    async fn forfeit_bond_skips_when_invoice_landed_concurrently() {
        // If a late `add_bond_invoice_action` persisted a `payout_invoice`
        // between the scheduler's snapshot and the forfeit UPDATE, the
        // CAS predicate (`AND payout_invoice IS NULL`) must hold the row
        // in `PendingPayout` so the next tick can route to
        // `pay_counterparty` instead of discarding the bolt11.
        let pool = setup_pool().await;
        let order_id = Uuid::new_v4();
        insert_order(&pool, order_id, maker_pk(), taker_pk()).await;
        let now = Utc::now().timestamp();
        let bond = pending_payout_bond(
            order_id,
            taker_pk(),
            10_000,
            5_000,
            now,
            Some("lnbc1pCONCURRENT"),
            None,
        );
        let bond = create_bond(&pool, bond).await.unwrap();

        forfeit_bond(&pool, &bond).await.unwrap();

        let after: (String,) = sqlx::query_as("SELECT state FROM bonds WHERE id = ?")
            .bind(bond.id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(after.0, BondState::PendingPayout.to_string());
    }

    #[tokio::test]
    async fn send_payment_failure_increments_attempts_and_flips_to_failed() {
        // After `max_retries` consecutive `send_payment` failures the
        // row must transition `PendingPayout -> Failed`. Exercises
        // `on_send_payment_failure` with a tight retry budget.
        let pool = setup_pool().await;
        let order_id = Uuid::new_v4();
        insert_order(&pool, order_id, maker_pk(), taker_pk()).await;
        let bond = pending_payout_bond(
            order_id,
            taker_pk(),
            10_000,
            5_000,
            Utc::now().timestamp(),
            Some("lnbc1pSOMETHING"),
            None,
        );
        let bond = create_bond(&pool, bond).await.unwrap();

        // First failure: attempts 0 -> 1, still PendingPayout.
        on_send_payment_failure(&pool, &bond, 3, "transient")
            .await
            .unwrap();
        let bond_after_1: Bond = sqlx::query_as("SELECT * FROM bonds WHERE id = ?")
            .bind(bond.id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(bond_after_1.payout_attempts, 1);
        assert_eq!(bond_after_1.state, BondState::PendingPayout.to_string());

        // Second + third failures use the *fresh* row each time so the
        // counter math is exercised end-to-end. Third must flip Failed.
        on_send_payment_failure(&pool, &bond_after_1, 3, "transient")
            .await
            .unwrap();
        let bond_after_2: Bond = sqlx::query_as("SELECT * FROM bonds WHERE id = ?")
            .bind(bond.id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(bond_after_2.payout_attempts, 2);

        on_send_payment_failure(&pool, &bond_after_2, 3, "transient")
            .await
            .unwrap();
        let bond_after_3: Bond = sqlx::query_as("SELECT * FROM bonds WHERE id = ?")
            .bind(bond.id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(bond_after_3.payout_attempts, 3);
        assert_eq!(bond_after_3.state, BondState::Failed.to_string());
    }

    #[tokio::test]
    async fn finalize_node_only_transitions_to_slashed() {
        // `slash_node_share_pct = 1.0` style row: counterparty share is
        // 0. `process_one_bond` routes to `finalize_node_only`, which is
        // now pure SQL — the HTLC was settled at slash time, so this
        // function just flips the state.
        let pool = setup_pool().await;
        let order_id = Uuid::new_v4();
        insert_order(&pool, order_id, maker_pk(), taker_pk()).await;
        let bond = pending_payout_bond(
            order_id,
            taker_pk(),
            10_000,
            10_000, // node_share == amount → counterparty_share = 0
            Utc::now().timestamp(),
            None,
            None,
        );
        let bond = create_bond(&pool, bond).await.unwrap();

        finalize_node_only(&pool, &bond).await.unwrap();

        let after: (String,) = sqlx::query_as("SELECT state FROM bonds WHERE id = ?")
            .bind(bond.id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(after.0, BondState::Slashed.to_string());
    }
}
