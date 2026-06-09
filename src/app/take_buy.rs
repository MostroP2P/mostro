use crate::app::bond;
use crate::app::bond::TakerContext;
use crate::app::context::AppContext;
use crate::util::{
    enqueue_order_msg, get_dev_fee, get_fiat_amount_requested, get_market_amount_and_fee,
    get_order, show_hold_invoice,
};

use crate::db::{seller_has_pending_order, update_user_trade_index};
use mostro_core::prelude::*;
use nostr_sdk::prelude::*;

pub async fn take_buy_action(
    ctx: &AppContext,
    msg: Message,
    event: &UnwrappedMessage,
    my_keys: &Keys,
) -> Result<(), MostroError> {
    let pool = ctx.pool();
    // Extract order ID from the message, returning an error if not found
    // Safe unwrap as we verified the message
    let mut order = get_order(&msg, pool).await?;

    // Get the request ID from the message
    let request_id = msg.get_inner_message_kind().request_id;

    // Check if the buyer has a pending order
    if seller_has_pending_order(pool, event.identity.to_string()).await? {
        return Err(MostroCantDo(CantDoReason::PendingOrderExists));
    }

    // Check if the order is a buy order and if its status is active
    if let Err(cause) = order.is_buy_order() {
        return Err(MostroCantDo(cause));
    };
    // Accept takes against orders in either `Pending` (no taker yet) or
    // `WaitingTakerBond` (Phase 1.5: a prior concurrent taker is
    // mid-bond). Both are pre-trade from the take-validation
    // perspective; the locked-bond gate inside the bond block below
    // catches the genuine post-trade case.
    if order.check_status(Status::Pending).is_err()
        && order.check_status(Status::WaitingTakerBond).is_err()
    {
        return Err(MostroCantDo(CantDoReason::InvalidOrderStatus));
    }

    // Validate that the order was sent from the correct maker
    order
        .not_sent_from_maker(event.sender)
        .map_err(MostroCantDo)?;

    // Anti-abuse bond (Phase 1, concurrent-bonds model). The take
    // handler doesn't release prior bonds at retake-time anymore —
    // multiple `Requested` taker bonds coexist on the order and the
    // first to reach `Locked` wins. We still need three guards here:
    //   1. A `Locked` *taker* bond already on the order means the
    //      trade is committed; reject with `PendingOrderExists`.
    //   2. The sender's own pubkey already has a `Requested` bond on
    //      this order → idempotent retry: re-send the same
    //      `PayInvoice` message and return.
    //   3. Otherwise fall through and create a fresh bond row.
    // We do *not* mutate the order's taker fields under the bond
    // path; that context lives on the bond row's `taker_*` columns
    // until the winning bond locks and
    // `on_bond_invoice_accepted` promotes it onto the order.
    //
    // Phase 5: with `apply_to = both` the maker's own bond is already
    // `Locked` on every published order — that is the normal state, not
    // a committed trade. The committed-trade gate must therefore only
    // count `Locked` *taker* bonds; otherwise a locked maker bond would
    // wrongly block every taker with `PendingOrderExists`.
    let bond_required = bond::taker_bond_required();
    if bond_required {
        let active = crate::app::bond::db::find_active_bonds_for_order(pool, order.id).await?;
        if bond::trade_committed_by_locked_taker_bond(&active) {
            return Err(MostroCantDo(CantDoReason::PendingOrderExists));
        }
        let sender_str = event.sender.to_string();
        if let Some(existing) = active.iter().find(|b| b.pubkey == sender_str) {
            if let Some(bolt11) = existing.payment_request.clone() {
                let order_kind = order.get_order_kind().map_err(MostroInternalErr)?;
                let bond_small = SmallOrder::new(
                    Some(order.id),
                    Some(order_kind),
                    Some(Status::Pending),
                    existing.amount_sats,
                    order.fiat_code.clone(),
                    order.min_amount,
                    order.max_amount,
                    existing.taker_fiat_amount.unwrap_or(order.fiat_amount),
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
                    Action::PayBondInvoice,
                    Some(Payload::PaymentRequest(Some(bond_small), bolt11, None)),
                    event.sender,
                    existing.taker_trade_index,
                )
                .await;
            }
            return Ok(());
        }
    }

    // Get the fiat amount requested by the user for range orders
    if let Some(am) = get_fiat_amount_requested(&order, &msg) {
        order.fiat_amount = am;
    } else {
        return Err(MostroCantDo(CantDoReason::OutOfRangeSatsAmount));
    }

    // If the order amount is zero, calculate the market price in sats
    if order.has_no_amount() {
        match get_market_amount_and_fee(order.fiat_amount, &order.fiat_code, order.premium).await {
            Ok(amount_fees) => {
                order.amount = amount_fees.0;
                order.fee = amount_fees.1;
                // Calculate dev_fee now that we know the fee amount
                let total_mostro_fee = order.fee * 2;
                order.dev_fee = get_dev_fee(total_mostro_fee);
            }
            Err(_) => return Err(MostroInternalErr(ServiceError::WrongAmountError)),
        };
    } else {
        // Calculate dev_fee for fixed price orders
        // The fee is already calculated at order creation, we only calculate dev_fee here
        let total_mostro_fee = order.fee * 2;
        order.dev_fee = get_dev_fee(total_mostro_fee);
    }

    // Get seller and buyer public keys
    let seller_pubkey = event.sender;
    let buyer_pubkey = order.get_buyer_pubkey().map_err(MostroInternalErr)?;

    // Resolve the trade index for this take (or 0 when identity == sender).
    let trade_index = match msg.get_inner_message_kind().trade_index {
        Some(trade_index) => trade_index,
        None => {
            if event.identity == event.sender {
                0
            } else {
                return Err(MostroInternalErr(ServiceError::InvalidPayload));
            }
        }
    };

    // Update trade index only after all checks are done. We bump
    // per-take (regardless of who wins the bond race) so the user's
    // monotonic trade-index counter stays consistent across attempts.
    update_user_trade_index(pool, event.identity.to_string(), trade_index)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

    // Concurrent-bonds path: stash this take's context on the bond
    // row, leave the order untouched. The winning bond's
    // `on_bond_invoice_accepted` callback will copy the `taker_*`
    // snapshot onto the order at lock-time and drive the trade flow.
    if bond_required {
        let taker_ctx = TakerContext {
            identity: event.identity.to_string(),
            trade_index,
            buyer_invoice: None,
            fiat_amount: order.fiat_amount,
            amount: order.amount,
            fee: order.fee,
            dev_fee: order.dev_fee,
        };
        bond::request_taker_bond(pool, &order, seller_pubkey, request_id, taker_ctx).await?;
        return Ok(());
    }

    // Non-bond path: legacy take. Persist the taker fields on the
    // order before driving the trade hold invoice.
    order.master_seller_pubkey = Some(event.identity.to_string());
    order.trade_index_seller = Some(trade_index);
    order.set_timestamp_now();

    // Show hold invoice and return success or error
    if let Err(cause) = show_hold_invoice(
        my_keys,
        None,
        &buyer_pubkey,
        &seller_pubkey,
        order,
        request_id,
    )
    .await
    {
        return Err(MostroInternalErr(ServiceError::HoldInvoiceError(
            cause.to_string(),
        )));
    }
    Ok(())
}
