use crate::util::{enqueue_order_msg, notify_taker_reputation};
use mostro_core::prelude::*;
use nostr_sdk::prelude::*;
use sqlx_crud::Crud;
use tracing::info;

pub async fn hold_invoice_paid(hash: &str, request_id: Option<u64>) -> Result<(), MostroError> {
    let pool = crate::db::connect()
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
    let order = crate::db::find_order_by_hash(&pool, hash)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
    let my_keys = crate::util::get_keys()
        .map_err(|e| MostroInternalErr(ServiceError::NostrError(e.to_string())))?;

    let buyer_pubkey = order
        .get_buyer_pubkey()
        .map_err(|e| MostroInternalErr(ServiceError::NostrError(e.to_string())))?;
    let seller_pubkey = order
        .get_seller_pubkey()
        .map_err(|e| MostroInternalErr(ServiceError::NostrError(e.to_string())))?;

    info!(
        "Order Id: {} - Seller paid invoice with hash: {hash}",
        order.id
    );

    // Check if the order kind is valid
    let order_kind = order.get_order_kind().map_err(MostroInternalErr)?;

    // We send this data related to the order to the parties
    let mut order_data = SmallOrder::new(
        Some(order.id),
        Some(order_kind),
        None,
        order.amount,
        order.fiat_code.clone(),
        order.min_amount,
        order.max_amount,
        order.fiat_amount,
        order.payment_method.clone(),
        order.premium,
        order.buyer_pubkey.as_ref().cloned(),
        order.seller_pubkey.as_ref().cloned(),
        None,
        Some(order.created_at),
        Some(order.expires_at),
        None,
        None,
    );
    let status;

    if order.buyer_invoice.is_some() {
        status = Status::Active;
        order_data.status = Some(status);
        // We send a confirmation message to seller
        enqueue_order_msg(
            request_id,
            Some(order.id),
            Action::BuyerTookOrder,
            Some(Payload::Order(order_data.clone())),
            seller_pubkey,
            None,
        )
        .await;
        // We send a message to buyer saying seller paid
        enqueue_order_msg(
            request_id,
            Some(order.id),
            Action::HoldInvoicePaymentAccepted,
            Some(Payload::Order(order_data)),
            buyer_pubkey,
            None,
        )
        .await;
    } else {
        let new_amount = order_data.amount - order.fee;
        order_data.amount = new_amount;
        status = Status::WaitingBuyerInvoice;
        order_data.status = Some(status);
        order_data.buyer_trade_pubkey = None;
        order_data.seller_trade_pubkey = None;
        // We ask to buyer for a new invoice
        enqueue_order_msg(
            request_id,
            Some(order.id),
            Action::AddInvoice,
            Some(Payload::Order(order_data)),
            buyer_pubkey,
            None,
        )
        .await;

        // We send a message to seller we are waiting for buyer invoice
        enqueue_order_msg(
            request_id,
            Some(order.id),
            Action::WaitingBuyerInvoice,
            None,
            seller_pubkey,
            None,
        )
        .await;

        // Notify taker reputation to maker
        tracing::info!("Notifying taker reputation to maker");
        notify_taker_reputation(&pool, &order).await?;
    }
    // We publish a new replaceable kind nostr event with the status updated
    // and update on local database the status and new event id
    if let Ok(updated_order) = crate::util::update_order_event(&my_keys, status, &order).await {
        // Update order on db
        let _ = updated_order.update(&pool).await;
    }

    // Update the invoice_held_at field
    crate::db::update_order_invoice_held_at_time(&pool, order.id, Timestamp::now().as_u64() as i64)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

    Ok(())
}

pub async fn hold_invoice_settlement(hash: &str) -> Result<()> {
    let pool = crate::db::connect().await?;
    let order = crate::db::find_order_by_hash(&pool, hash).await?;
    info!(
        "Order Id: {} - Invoice with hash: {} was settled!",
        order.id, hash
    );
    Ok(())
}

pub async fn hold_invoice_canceled(hash: &str) -> Result<()> {
    let pool = crate::db::connect().await?;
    let order = crate::db::find_order_by_hash(&pool, hash).await?;
    info!(
        "Order Id: {} - Invoice with hash: {} was canceled!",
        order.id, hash
    );
    Ok(())
}
