//! What mostro does with a trade whose escrow is about to expire.
//!
//! A trade is escrowed in a hold HTLC, and that HTLC has a hard lifetime: LND
//! force-cancels it a few blocks before its CLTV expiry height
//! (`invoices.holdexpirydelta`) so the channel is never forced to close over a
//! held payment. The refund goes to the seller no matter what the trade was
//! doing at that moment.
//!
//! Nothing in the order state machine used to know about that horizon, so a
//! trade could sit in `active` or `fiat-sent` until the escrow silently went
//! back to the seller — leaving an order that still reads as live, a buyer who
//! can still send fiat, and a release that fails at settle.
//!
//! This module is the policy that runs ahead of LND, driven by the scheduler
//! sweep. It is deliberately asymmetric about who may lose their escrow:
//!
//! - `active` — nobody has claimed to have paid fiat. Cancelling the trade
//!   costs no one anything the CLTV would not have taken anyway, so mostro
//!   ends it in a controlled way at the safety window: the seller is refunded
//!   by *our* cancel, both parties are told, and the order stops advertising
//!   itself as tradeable.
//! - `fiat-sent` — the buyer says the fiat is gone. Cancelling here would hand
//!   the seller both the fiat and the escrow, so mostro never does it. It
//!   opens the dispute instead, at the earlier warning window, which is the
//!   only action that can still save the buyer: a solver can settle to them
//!   while the escrow exists.
//! - `dispute` — a solver is already on it and can settle up to the last
//!   block. Taking the escrow away early would remove the only outcome that
//!   pays the buyer, so mostro alerts the operator and lets the horizon
//!   arrive; [`crate::flow::hold_invoice_canceled`] then closes the order
//!   cleanly when LND cancels, so no admin action can fail at settle later.

use crate::app::bond;
use crate::app::context::AppContext;
use crate::app::dispute::open_deadline_dispute;
use crate::config::types::LightningSettings;
use crate::flow::notify_escrow_canceled;
use crate::lightning::LndConnector;
use crate::util::update_order_event;
use mostro_core::prelude::*;

/// What the sweep should do with one order this tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EscrowDeadlineAction {
    /// Still far enough from the horizon to leave alone.
    None,
    /// Cancel the hold invoice and close the order (`active` only).
    CancelTrade,
    /// Open the dispute so a solver can act while the escrow lasts.
    OpenDispute,
    /// Tell the operator a disputed trade is running out of escrow. `urgent`
    /// separates the first warning from the one inside the safety window.
    AlertSolvers { urgent: bool },
}

/// Decide what to do with an order that is `remaining_blocks` away from its
/// escrow HTLC's expiry height.
///
/// Pure on purpose: the whole policy table is testable without an LND node or
/// a database, and the sweep around it stays a loop over I/O.
///
/// `remaining_blocks` is signed because the horizon can already be behind us —
/// LND may be lagging, or the daemon may have been down through it — and a
/// past-due escrow must read as "most urgent", not as a huge unsigned value.
pub fn escrow_deadline_action(
    status: Status,
    remaining_blocks: i64,
    settings: &LightningSettings,
) -> EscrowDeadlineAction {
    let safety = settings.escrow_expiry_safety_blocks as i64;
    let warning = settings.escrow_expiry_warning_blocks as i64;

    if remaining_blocks > warning {
        return EscrowDeadlineAction::None;
    }

    match status {
        // The buyer has not claimed anything yet: no fiat leg to protect, so
        // the trade can be ended cleanly. Waiting until the safety window
        // gives a slow-but-honest trade the whole warning band to finish.
        Status::Active if remaining_blocks <= safety => EscrowDeadlineAction::CancelTrade,
        Status::Active => EscrowDeadlineAction::None,
        // Escalate as soon as the warning window opens: a dispute is only
        // useful if a solver has time to act on it before the escrow goes.
        Status::FiatSent => EscrowDeadlineAction::OpenDispute,
        Status::Dispute => EscrowDeadlineAction::AlertSolvers {
            urgent: remaining_blocks <= safety,
        },
        // Any other status has no escrow to lose; the sweep does not select
        // them, and a race that moved one here mid-tick is a no-op.
        _ => EscrowDeadlineAction::None,
    }
}

/// Carry out the decision [`escrow_deadline_action`] returned.
///
/// Split from the decision so the sweep can drop an alert it has already
/// emitted for this order before anything is executed — the alerts are the
/// only action that would otherwise repeat on every tick.
pub async fn apply_escrow_deadline_action(
    ctx: &AppContext,
    ln_client: &mut LndConnector,
    order: &Order,
    action: EscrowDeadlineAction,
    remaining_blocks: i64,
) {
    match action {
        EscrowDeadlineAction::None => {}
        EscrowDeadlineAction::CancelTrade => {
            cancel_expiring_trade(ctx, ln_client, order, remaining_blocks).await;
        }
        EscrowDeadlineAction::OpenDispute => {
            if let Err(e) = open_deadline_dispute(ctx, order).await {
                // Left in `fiat-sent`, so the next tick tries again — there is
                // still time, and giving up would be giving up on the buyer.
                tracing::error!(
                    order_id = %order.id,
                    remaining_blocks,
                    "Could not open the escrow-deadline dispute ({e}); retrying next tick"
                );
            }
        }
        EscrowDeadlineAction::AlertSolvers { urgent } => {
            tracing::error!(
                order_id = %order.id,
                remaining_blocks,
                urgent,
                "Disputed order is running out of escrow: the hold invoice expires in \
                 {remaining_blocks} blocks and LND will refund the seller then. A solver \
                 has to settle or cancel before that height"
            );
        }
    }
}

/// Cancel the escrow of a trade that ran out of time and close the order.
///
/// The hold invoice is cancelled *before* the order is touched, exactly as the
/// waiting-order timeout does: on failure the order keeps its status and stays
/// in the sweep, so the next tick retries instead of leaving a canceled order
/// backed by an HTLC that is still encumbered. If the daemon dies between the
/// two, LND's cancel event reaches
/// [`crate::flow::hold_invoice_canceled`], which closes the order the same way.
async fn cancel_expiring_trade(
    ctx: &AppContext,
    ln_client: &mut LndConnector,
    order: &Order,
    remaining_blocks: i64,
) {
    let Some(hash) = order.hash.as_ref() else {
        tracing::warn!(order_id = %order.id, "Order reached its escrow deadline with no payment hash");
        return;
    };

    if let Err(e) = ln_client.cancel_hold_invoice(hash).await {
        tracing::error!(
            order_id = %order.id,
            "escrow_deadline: could not cancel the hold invoice ({e}); \
             leaving the order untouched so the next tick retries"
        );
        return;
    }

    tracing::warn!(
        order_id = %order.id,
        remaining_blocks,
        "Trade reached its escrow deadline: funds returned to the seller and the order closed"
    );

    let order_updated = match update_order_event(ctx.keys(), Status::Canceled, order).await {
        Ok(updated) => updated,
        Err(e) => {
            // The escrow is already refunded, so the order must not be left
            // advertising itself as live. The subscriber's reaction to the
            // cancel we just made is the backstop that closes it.
            tracing::error!(
                order_id = %order.id,
                "escrow_deadline: hold invoice canceled but publishing the update failed ({e})"
            );
            return;
        }
    };

    match crate::db::claim_escrow_order_canceled(ctx.pool(), order.id, &order_updated.event_id)
        .await
    {
        Ok(true) => {}
        Ok(false) => {
            // The subscriber won the race on the cancel we just made, or a
            // user action resolved the order first. Either way it is closed.
            tracing::info!(order_id = %order.id, "Order was already resolved by another path");
            return;
        }
        Err(e) => {
            tracing::error!(
                order_id = %order.id,
                "escrow_deadline: could not persist the cancelation ({e})"
            );
            return;
        }
    }

    notify_escrow_canceled(order).await;
    bond::release_taker_bonds_for_order_or_warn(ctx.pool(), order.id, "escrow_deadline").await;
    bond::resolve_range_maker_bond_at_close_or_warn(ctx.pool(), order, "escrow_deadline").await;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shipped windows: escalate 72 blocks out, cancel 36 blocks out.
    fn settings() -> LightningSettings {
        LightningSettings {
            hold_invoice_cltv_delta: 144,
            escrow_expiry_warning_blocks: 72,
            escrow_expiry_safety_blocks: 36,
            ..Default::default()
        }
    }

    fn action(status: Status, remaining: i64) -> EscrowDeadlineAction {
        escrow_deadline_action(status, remaining, &settings())
    }

    #[test]
    fn a_fresh_trade_is_left_alone() {
        // Just paid: 144 blocks of escrow, nothing to do for the next ~12 h.
        for status in [Status::Active, Status::FiatSent, Status::Dispute] {
            assert_eq!(action(status, 144), EscrowDeadlineAction::None);
            assert_eq!(action(status, 73), EscrowDeadlineAction::None);
        }
    }

    #[test]
    fn an_active_trade_is_canceled_only_at_the_safety_window() {
        // The warning band belongs to the traders: an active order has no
        // fiat leg at risk, so it gets every block of it before being ended.
        assert_eq!(action(Status::Active, 72), EscrowDeadlineAction::None);
        assert_eq!(action(Status::Active, 37), EscrowDeadlineAction::None);
        assert_eq!(
            action(Status::Active, 36),
            EscrowDeadlineAction::CancelTrade
        );
        assert_eq!(action(Status::Active, 1), EscrowDeadlineAction::CancelTrade);
    }

    #[test]
    fn a_fiat_sent_trade_escalates_at_the_warning_window() {
        // Earlier than the cancel, because a dispute is only worth opening
        // while a solver still has time to settle it.
        assert_eq!(
            action(Status::FiatSent, 72),
            EscrowDeadlineAction::OpenDispute
        );
        assert_eq!(
            action(Status::FiatSent, 36),
            EscrowDeadlineAction::OpenDispute
        );
        assert_eq!(
            action(Status::FiatSent, 1),
            EscrowDeadlineAction::OpenDispute
        );
    }

    #[test]
    fn a_fiat_sent_trade_is_never_canceled_by_the_deadline() {
        // Cancelling would hand the seller the fiat and the sats. Whatever
        // the remaining blocks, the answer stays the dispute.
        for remaining in [72, 36, 10, 0, -50] {
            assert_ne!(
                action(Status::FiatSent, remaining),
                EscrowDeadlineAction::CancelTrade,
                "a declared fiat leg must never be cancelled by the daemon"
            );
        }
    }

    #[test]
    fn a_disputed_trade_alerts_and_keeps_its_escrow() {
        // The solver can settle up to the last block, so mostro only escalates
        // the alert as the horizon closes in.
        assert_eq!(
            action(Status::Dispute, 72),
            EscrowDeadlineAction::AlertSolvers { urgent: false }
        );
        assert_eq!(
            action(Status::Dispute, 37),
            EscrowDeadlineAction::AlertSolvers { urgent: false }
        );
        assert_eq!(
            action(Status::Dispute, 36),
            EscrowDeadlineAction::AlertSolvers { urgent: true }
        );
    }

    #[test]
    fn a_past_due_escrow_is_the_most_urgent_case() {
        // Negative remaining blocks mean the horizon is behind us — after a
        // long outage, say. It must read as past the deadline, not wrap into
        // a distant future.
        assert_eq!(
            action(Status::Active, -10),
            EscrowDeadlineAction::CancelTrade
        );
        assert_eq!(
            action(Status::FiatSent, -10),
            EscrowDeadlineAction::OpenDispute
        );
        assert_eq!(
            action(Status::Dispute, -10),
            EscrowDeadlineAction::AlertSolvers { urgent: true }
        );
    }

    #[test]
    fn statuses_without_escrow_are_never_acted_on() {
        // The sweep does not select them; this pins that a race that moves an
        // order out from under it mid-tick cannot trigger an action either.
        for status in [
            Status::Pending,
            Status::WaitingPayment,
            Status::SettledHoldInvoice,
            Status::Success,
            Status::Canceled,
        ] {
            assert_eq!(action(status, 0), EscrowDeadlineAction::None);
        }
    }
}
