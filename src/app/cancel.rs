use crate::db::{edit_pubkeys_order, update_order_to_initial_state};
use crate::lightning::LndConnector;
use crate::util::{enqueue_order_msg, get_order, update_order_event};
use mostro_core::prelude::*;
use nostr::nips::nip59::UnwrappedGift;
use nostr_sdk::prelude::*;
use sqlx::{Pool, Sqlite};
use sqlx_crud::Crud;
use std::str::FromStr;
use tracing::info;

/// Reset API-provided quote-derived amounts when republishing an order.
///
/// When an order was created with `price_from_api`, its `amount` and `fee`
/// are derived from a volatile quote. If the order is republished (e.g. after
/// cancellation by one party), we clear those values so that the next publish
/// cycle recalculates them with a fresh price.
fn reset_api_quotes(order: &mut Order) {
    if order.price_from_api {
        order.amount = 0;
        order.fee = 0;
        // Also reset dev fee to ensure fresh recalculation on re-take
        order.dev_fee = 0;
    }
}

/// Notify the order creator that the order has been republished with updated state.
///
/// This is used after certain cancellation flows where the order returns to a
/// publishable state and the creator should see the updated `Status`.
async fn notify_creator(order: &Order, request_id: Option<u64>) -> Result<(), MostroError> {
    // Get creator pubkey
    let creator_pubkey = order.get_creator_pubkey().map_err(MostroInternalErr)?;

    enqueue_order_msg(
        request_id,
        Some(order.id),
        Action::NewOrder,
        Some(Payload::Order(SmallOrder::from(order.clone()))),
        creator_pubkey,
        None,
    )
    .await;

    Ok(())
}

/// Cancel a cooperative execution
/// Step 2 of a cooperative cancel flow: both parties have signaled intent.
///
/// - Cancels the hold invoice if present (funds go back to seller)
/// - Persists `Status::CooperativelyCanceled`
/// - Publishes a new replaceable nostr event and notifies both parties
async fn cancel_cooperative_execution_step_2(
    pool: &Pool<Sqlite>,
    event: &UnwrappedGift,
    request_id: Option<u64>,
    mut order: Order,
    counterparty_pubkey: String,
    my_keys: &Keys,
    ln_client: &mut LndConnector,
) -> Result<(), MostroError> {
    // Guard: the same party cannot both initiate and confirm the cooperative cancel.
    if let Some(initiator) = &order.cancel_initiator_pubkey {
        if *initiator == event.rumor.pubkey.to_string() {
            // We create a Message
            return Err(MostroCantDo(CantDoReason::InvalidPubkey));
        }
    }

    // Cancel hold invoice if present; if funds were locked, this returns them to the seller.
    if let Some(hash) = &order.hash {
        // We return funds to seller
        ln_client.cancel_hold_invoice(hash).await?;
        info!(
            "Cooperative cancel: Order Id {}: Funds returned to seller",
            &order.id
        );
    }
    order.status = Status::CooperativelyCanceled.to_string();
    // update db
    let order = order
        .clone()
        .update(pool)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
    // Publish a replaceable nostr event reflecting the new status and persist the mapping.
    update_order_event(my_keys, Status::CooperativelyCanceled, &order)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::NostrError(e.to_string())))?;
    // We create a Message for an accepted cooperative cancel and send it to both parties
    enqueue_order_msg(
        request_id,
        Some(order.id),
        Action::CooperativeCancelAccepted,
        None,
        event.rumor.pubkey,
        None,
    )
    .await;
    let counterparty_pubkey = PublicKey::from_str(&counterparty_pubkey)
        .map_err(|_| MostroInternalErr(ServiceError::InvalidPubkey))?;
    enqueue_order_msg(
        None,
        Some(order.id),
        Action::CooperativeCancelAccepted,
        None,
        counterparty_pubkey,
        None,
    )
    .await;
    info!("Cancel: Order Id {} canceled cooperatively!", order.id);

    Ok(())
}

/// Step 1 of a cooperative cancel flow: first party signals intent.
///
/// - Records the initiator's pubkey
/// - Notifies both parties so the counterparty can confirm (step 2)
async fn cancel_cooperative_execution_step_1(
    pool: &Pool<Sqlite>,
    event: &UnwrappedGift,
    mut order: Order,
    counterparty_pubkey: String,
    request_id: Option<u64>,
) -> Result<(), MostroError> {
    order.cancel_initiator_pubkey = Some(event.rumor.pubkey.to_string());
    // update db
    let order = order
        .update(pool)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
    // Notify both parties: initiator sees "initiated by you" and the counterparty sees
    // "initiated by peer".
    enqueue_order_msg(
        request_id,
        Some(order.id),
        Action::CooperativeCancelInitiatedByYou,
        None,
        event.rumor.pubkey,
        None,
    )
    .await;
    let counterparty_pubkey = PublicKey::from_str(&counterparty_pubkey)
        .map_err(|_| MostroInternalErr(ServiceError::InvalidPubkey))?;
    enqueue_order_msg(
        None,
        Some(order.id),
        Action::CooperativeCancelInitiatedByPeer,
        None,
        counterparty_pubkey,
        None,
    )
    .await;

    Ok(())
}

/// Cancel an order by the taker
/// Cancellation path when the taker cancels a not-yet-active order.
///
/// - If a hold invoice exists, cancel it (refund to seller)
/// - Notify the taker
/// - Reset quote-derived amounts (if any) and return order to initial state
/// - Notify the maker/creator that the order is republished
async fn cancel_order_by_taker(
    pool: &Pool<Sqlite>,
    event: &UnwrappedGift,
    mut order: Order,
    my_keys: &Keys,
    request_id: Option<u64>,
    ln_client: &mut LndConnector,
    taker_pubkey: PublicKey,
) -> Result<(), MostroError> {
    // Cancel hold invoice if present
    if let Some(hash) = &order.hash {
        ln_client.cancel_hold_invoice(hash).await?;
        info!("Order Id {}: Funds returned to seller", &order.id);
    }

    //We notify the taker that the order is cancelled
    enqueue_order_msg(
        request_id,
        Some(order.id),
        Action::Canceled,
        None,
        event.rumor.pubkey,
        None,
    )
    .await;

    // Reset api quotes
    reset_api_quotes(&mut order);

    // Update order to initial state and save it to the database
    update_order_to_initial_state(pool, order.id, order.amount, order.fee)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

    // Clean stored pubkeys for this order; republish will set them anew.
    let order = edit_pubkeys_order(pool, &order)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

    let order_updated = update_order_event(my_keys, Status::Pending, &order)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::NostrError(e.to_string())))?;

    info!(
        "{}: Canceled order Id {} republishing order",
        taker_pubkey, order.id
    );

    // Notify the creator about the republished order after the taker-side cancellation flow completes
    notify_creator(&order_updated, request_id).await?;

    Ok(())
}

/// Cancel an order by the maker
/// Cancellation path when the maker cancels a not-yet-active order.
///
/// - Publishes `Status::Canceled` and persists it
/// - Cancels any hold invoice
/// - Notifies both parties
async fn cancel_order_by_maker(
    pool: &Pool<Sqlite>,
    event: &UnwrappedGift,
    order: Order,
    taker_pubkey: PublicKey,
    my_keys: &Keys,
    request_id: Option<u64>,
    ln_client: &mut LndConnector,
) -> Result<(), MostroError> {
    // We publish a new replaceable kind nostr event with the status updated
    if let Ok(order_updated) = update_order_event(my_keys, Status::Canceled, &order).await {
        order_updated
            .update(pool)
            .await
            .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
    }
    // Cancel hold invoice if present
    if let Some(hash) = &order.hash {
        ln_client.cancel_hold_invoice(hash).await?;
        info!("Order Id {}: Funds returned to seller", &order.id);
    }

    enqueue_order_msg(
        request_id,
        Some(order.id),
        Action::Canceled,
        None,
        event.rumor.pubkey,
        None,
    )
    .await;
    //We notify the taker that the order was cancelled
    enqueue_order_msg(
        None,
        Some(order.id),
        Action::Canceled,
        None,
        taker_pubkey,
        None,
    )
    .await;

    Ok(())
}

/// Cancel a `Pending` order by the maker before it becomes active.
///
/// This updates the replaceable event to `Status::Canceled`, persists it, and
/// notifies the maker. No invoice is involved yet in this state.
async fn cancel_pending_order_from_maker(
    pool: &Pool<Sqlite>,
    event: &UnwrappedGift,
    order: &mut Order,
    my_keys: &Keys,
    request_id: Option<u64>,
) -> Result<(), MostroError> {
    // Validates if this user is the order creator
    order
        .sent_from_maker(event.rumor.pubkey)
        .map_err(|_| MostroCantDo(CantDoReason::IsNotYourOrder))?;
    // Publish a replaceable nostr event with updated status and persist it.
    match update_order_event(my_keys, Status::Canceled, order).await {
        Ok(order_updated) => {
            order_updated
                .update(pool)
                .await
                .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
        }
        Err(e) => {
            return Err(MostroInternalErr(ServiceError::DbAccessError(
                e.to_string(),
            )));
        }
    }
    // We create a Message for cancel
    enqueue_order_msg(
        request_id,
        Some(order.id),
        Action::Canceled,
        None,
        event.rumor.pubkey,
        None,
    )
    .await;
    Ok(())
}

/// Top-level cancel entrypoint.
///
/// Routes to one of the specific flows based on the current `Status` and who
/// sent the request:
/// - Pending: maker-only soft cancel (republish)
/// - WaitingPayment/WaitingBuyerInvoice: not-active flows
/// - Active/FiatSent/Dispute: cooperative flows between buyer and seller
pub async fn cancel_action(
    msg: Message,
    event: &UnwrappedGift,
    my_keys: &Keys,
    pool: &Pool<Sqlite>,
    ln_client: &mut LndConnector,
) -> Result<(), MostroError> {
    // Get request id
    let request_id = msg.get_inner_message_kind().request_id;
    // Get order id
    let mut order = get_order(&msg, pool).await?;

    // Short-circuit if already canceled in any terminal-cancel state.
    if order.check_status(Status::Canceled).is_ok()
        || order.check_status(Status::CooperativelyCanceled).is_ok()
        || order.check_status(Status::CanceledByAdmin).is_ok()
    {
        return Err(MostroCantDo(CantDoReason::OrderAlreadyCanceled));
    }

    // Pending: maker can revert to Canceled state and republish without cooperative steps.
    if order.check_status(Status::Pending).is_ok() {
        cancel_pending_order_from_maker(pool, event, &mut order, my_keys, request_id).await?;
        return Ok(());
    }

    // Do the appropriate cancellation flow based on the order status
    // Route to the appropriate cancellation flow based on active vs not-active states.
    match order.get_order_status().map_err(MostroInternalErr)? {
        Status::WaitingPayment | Status::WaitingBuyerInvoice => {
            cancel_not_active_order(pool, event, order, my_keys, request_id, ln_client).await?
        }
        Status::Active | Status::FiatSent | Status::Dispute => {
            cancel_active_order(pool, event, order, my_keys, request_id, ln_client).await?
        }
        _ => return Err(MostroCantDo(CantDoReason::NotAllowedByStatus)),
    }

    Ok(())
}

/// Cancellation router for active trades.
///
/// Marks which side initiated the cooperative cancel and either starts the flow
/// (step 1) or completes it (step 2) when both sides have acknowledged.
async fn cancel_active_order(
    pool: &Pool<Sqlite>,
    event: &UnwrappedGift,
    mut order: Order,
    my_keys: &Keys,
    request_id: Option<u64>,
    ln_client: &mut LndConnector,
) -> Result<(), MostroError> {
    // Get seller and buyer pubkey
    let seller_pubkey = order.get_seller_pubkey().map_err(MostroInternalErr)?;
    let buyer_pubkey = order.get_buyer_pubkey().map_err(MostroInternalErr)?;

    let counterparty_pubkey: String;
    if buyer_pubkey == event.rumor.pubkey {
        order.buyer_cooperativecancel = true;
        counterparty_pubkey = seller_pubkey.to_string();
    } else {
        order.seller_cooperativecancel = true;
        counterparty_pubkey = buyer_pubkey.to_string();
    }

    // If there is already an initiator recorded, this call becomes the confirmation (step 2).
    match order.cancel_initiator_pubkey {
        Some(_) => {
            cancel_cooperative_execution_step_2(
                pool,
                event,
                request_id,
                order,
                counterparty_pubkey,
                my_keys,
                ln_client,
            )
            .await?;
        }
        None => {
            cancel_cooperative_execution_step_1(
                pool,
                event,
                order,
                counterparty_pubkey,
                request_id,
            )
            .await?;
        }
    }
    Ok(())
}

/// Cancellation router for not-yet-active trades.
///
/// If the maker sent the event, run the maker path; otherwise, only the taker
/// can cancel. This ensures the correct party authorization for early cancels.
async fn cancel_not_active_order(
    pool: &Pool<Sqlite>,
    event: &UnwrappedGift,
    order: Order,
    my_keys: &Keys,
    request_id: Option<u64>,
    ln_client: &mut LndConnector,
) -> Result<(), MostroError> {
    // Get seller and buyer pubkey
    let seller_pubkey = order.get_seller_pubkey().map_err(MostroInternalErr)?;
    let buyer_pubkey = order.get_buyer_pubkey().map_err(MostroInternalErr)?;

    // Get order taker pubkey
    let taker_pubkey = if order.creator_pubkey == seller_pubkey.to_string() {
        buyer_pubkey
    } else if order.creator_pubkey == buyer_pubkey.to_string() {
        seller_pubkey
    } else {
        return Err(MostroInternalErr(ServiceError::InvalidPubkey));
    };

    if order.sent_from_maker(event.rumor.pubkey).is_ok() {
        cancel_order_by_maker(
            pool,
            event,
            order,
            taker_pubkey,
            my_keys,
            request_id,
            ln_client,
        )
        .await?;
    } else if event.rumor.pubkey == taker_pubkey {
        cancel_order_by_taker(
            pool,
            event,
            order,
            my_keys,
            request_id,
            ln_client,
            taker_pubkey,
        )
        .await?;
    } else {
        return Err(MostroCantDo(CantDoReason::InvalidPubkey));
    }
    Ok(())
}
