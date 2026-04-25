use crate::app::bond;
use crate::app::bond::db::find_active_bonds_for_order;
use crate::app::context::AppContext;
use crate::util::{
    get_dev_fee, get_fiat_amount_requested, get_market_amount_and_fee, get_order, show_hold_invoice,
};

use crate::db::{seller_has_pending_order, update_user_trade_index};
use mostro_core::prelude::*;
use nostr_sdk::prelude::*;
use sqlx_crud::Crud;

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
    // Check if the order status is pending
    if let Err(cause) = order.check_status(Status::Pending) {
        return Err(MostroCantDo(cause));
    }

    // Validate that the order was sent from the correct maker
    order
        .not_sent_from_maker(event.sender)
        .map_err(MostroCantDo)?;

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

    // Add seller identity and trade index to the order
    order.master_seller_pubkey = Some(event.identity.to_string());
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
    order.trade_index_seller = Some(trade_index);

    // Timestamp the order take time
    order.set_timestamp_now();

    // Update trade index only after all checks are done
    update_user_trade_index(pool, event.identity.to_string(), trade_index)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

    // Anti-abuse bond (Phase 1): if the operator opted into a taker bond,
    // intercept the take here. We persist the partially-populated order
    // (status stays `Pending`) and request the bond. The trade hold
    // invoice is created later — once the bond locks — by the bond
    // subscriber's continuation in `bond::flow::resume_take_after_bond`.
    if bond::taker_bond_required() {
        // Defend against concurrent takes for the same order: if another
        // taker already has an active bond on this order, the second take
        // must back off rather than create a duplicate bond row.
        let existing = find_active_bonds_for_order(pool, order.id).await?;
        if !existing.is_empty() {
            return Err(MostroCantDo(CantDoReason::PendingOrderExists));
        }

        // Stash the seller (taker) trade pubkey so the post-bond
        // continuation can resume `show_hold_invoice` with the same
        // arguments the legacy path would have used.
        order.seller_pubkey = Some(seller_pubkey.to_string());

        let persisted = order
            .update(pool)
            .await
            .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

        bond::request_taker_bond(
            pool,
            &persisted,
            seller_pubkey,
            request_id,
            Some(trade_index),
        )
        .await?;
        return Ok(());
    }

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
