use crate::util::{
    enqueue_order_msg, get_order, notify_taker_reputation, show_hold_invoice, update_order_event,
    validate_invoice,
};
use mostro_core::prelude::*;
use nostr::nips::nip59::UnwrappedGift;
use nostr_sdk::prelude::*;
use sqlx::{Pool, Sqlite};
use sqlx_crud::Crud;

pub async fn pay_new_invoice(
    order: &mut Order,
    pool: &Pool<Sqlite>,
    msg: &Message,
) -> Result<(), MostroError> {
    order.payment_attempts = 0;
    order
        .clone()
        .update(pool)
        .await
        .map_err(|cause| MostroInternalErr(ServiceError::DbAccessError(cause.to_string())))?;
    enqueue_order_msg(
        msg.get_inner_message_kind().request_id,
        Some(order.id),
        Action::InvoiceUpdated,
        None,
        order.get_buyer_pubkey().map_err(MostroInternalErr)?,
        None,
    )
    .await;
    Ok(())
}

pub async fn add_invoice_action(
    msg: Message,
    event: &UnwrappedGift,
    my_keys: &Keys,
    pool: &Pool<Sqlite>,
) -> Result<(), MostroError> {
    // Get order
    let mut order = get_order(&msg, pool).await?;
    // Check order status
    let ord_status = order.get_order_status().map_err(MostroInternalErr)?;
    // Check order kind
    order.get_order_kind().map_err(MostroInternalErr)?;
    // Get buyer pubkey
    let buyer_pubkey = order.get_buyer_pubkey().map_err(MostroInternalErr)?;
    // Only the buyer can add an invoice
    if buyer_pubkey != event.rumor.pubkey {
        return Err(MostroCantDo(CantDoReason::InvalidPeer));
    }
    // We save the invoice on db
    order.buyer_invoice = validate_invoice(&msg, &order).await?;
    // Buyer can add invoice orders with WaitingBuyerInvoice status
    match ord_status {
        Status::SettledHoldInvoice => {
            pay_new_invoice(&mut order, pool, &msg).await?;
            return Ok(());
        }
        Status::WaitingBuyerInvoice => {}
        _ => {
            return Err(MostroCantDo(CantDoReason::NotAllowedByStatus));
        }
    }

    // Notify taker reputation
    tracing::info!("Notifying taker reputation to maker");
    notify_taker_reputation(pool, &order).await?;

    // Get seller pubkey
    let seller_pubkey = order.get_seller_pubkey().map_err(MostroInternalErr)?;
    // Check if the order has a preimage
    if order.preimage.is_some() {
        // We publish a new replaceable kind nostr event with the status updated
        // and update on local database the status and new event id
        let active_order = match update_order_event(my_keys, Status::Active, &order).await {
            Ok(updated_order) => {
                // Update in database
                updated_order.clone().update(pool).await.map_err(|cause| {
                    MostroInternalErr(ServiceError::DbAccessError(cause.to_string()))
                })?;
                updated_order
            }
            Err(e) => return Err(e),
        };

        // We send a confirmation message to seller
        let mut seller_order = SmallOrder::from(active_order.clone());
        seller_order.amount = active_order.amount.saturating_add(active_order.fee);
        // Clear buyer_invoice to avoid leaking buyer's payment info to seller
        seller_order.buyer_invoice = None;
        enqueue_order_msg(
            None,
            Some(active_order.id),
            Action::BuyerTookOrder,
            Some(Payload::Order(seller_order)),
            seller_pubkey,
            None,
        )
        .await;
        // We send a message to buyer saying seller paid
        let mut buyer_order = SmallOrder::from(active_order.clone());
        buyer_order.amount = active_order.amount.saturating_sub(active_order.fee);
        enqueue_order_msg(
            msg.get_inner_message_kind().request_id,
            Some(active_order.id),
            Action::HoldInvoicePaymentAccepted,
            Some(Payload::Order(buyer_order)),
            buyer_pubkey,
            None,
        )
        .await;
    } else if let Err(cause) = show_hold_invoice(
        my_keys,
        None,
        &buyer_pubkey,
        &seller_pubkey,
        order,
        msg.get_inner_message_kind().request_id,
    )
    .await
    {
        return Err(MostroInternalErr(ServiceError::HoldInvoiceError(
            cause.to_string(),
        )));
    }
    Ok(())
}
