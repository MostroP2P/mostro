use crate::app::bond;
use crate::app::bond::supersede_prior_taker_bonds;
use crate::app::context::AppContext;
use crate::util::{
    get_dev_fee, get_fiat_amount_requested, get_market_amount_and_fee, get_order,
    show_hold_invoice, update_order_event,
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
    // Check if the order status is pre-trade. After Phase 1.5,
    // `WaitingTakerBond` is the daemon-internal "matched, awaiting
    // bond" state; it remains takeable on the wire (NIP-69 `pending`)
    // per the non-blockability invariant, and `supersede_prior_taker_bonds`
    // below rejects only when a prior bond is already `Locked`. Both
    // statuses are pre-trade; either is a valid entry point for take.
    if order.check_status(Status::Pending).is_err()
        && order.check_status(Status::WaitingTakerBond).is_err()
    {
        return Err(MostroCantDo(CantDoReason::InvalidOrderStatus));
    }

    // Validate that the order was sent from the correct maker
    order
        .not_sent_from_maker(event.sender)
        .map_err(MostroCantDo)?;

    // Anti-abuse bond reconciliation: always run the supersede pass
    // before this take proceeds, regardless of the current
    // `taker_bond_required()` flag. The reason is that bonds may
    // have been enabled at the time a *prior* taker took this order
    // — leaving a `Requested` or `Locked` bond row attached — and
    // then disabled before the current take arrives. Gating on
    // `taker_bond_required()` would silently skip that
    // reconciliation: a `Locked` prior bond would no longer block
    // the take (regression vs. the rule that locked bonds mean the
    // trade is committed), and a `Requested` prior bond would be
    // orphaned in the DB. The helper is a no-op (returns `Ok(0)`)
    // when no active bond exists for the order, so always-calling
    // is safe and cheap. Done before the market-price recomputation
    // below so re-takes of API-priced orders see a fresh quote.
    let bond_required = bond::taker_bond_required();
    let superseded = supersede_prior_taker_bonds(pool, order.id, event.sender).await?;
    if superseded > 0 && order.price_from_api {
        order.amount = 0;
        order.fee = 0;
        order.dev_fee = 0;
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

    // Anti-abuse bond (Phase 1.5): if the operator opted into a taker
    // bond, intercept the take here. The order's status flips to
    // `WaitingTakerBond` while we wait for the taker to lock the bond
    // hold invoice. The wire-published NIP-33 status stays `pending`
    // per the non-blockability invariant (see
    // `nip33::create_status_tags`); the trade hold invoice is created
    // later — once the bond locks — by `bond::flow::resume_take_after_bond`.
    if bond_required {
        // Stash the seller (taker) trade pubkey so the post-bond
        // continuation can resume `show_hold_invoice` with the same
        // arguments the legacy path would have used.
        order.seller_pubkey = Some(seller_pubkey.to_string());

        // Republish NIP-33 with `WaitingTakerBond` (which maps back
        // to NIP-69 `pending` for the wire), then persist.
        let order_updated = update_order_event(my_keys, Status::WaitingTakerBond, &order)
            .await
            .map_err(|e| MostroInternalErr(ServiceError::NostrError(e.to_string())))?;

        let persisted = order_updated
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
