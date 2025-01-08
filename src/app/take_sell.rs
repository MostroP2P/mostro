use crate::lightning::invoice::is_valid_invoice;
use crate::util::{
    get_fiat_amount_requested, get_market_amount_and_fee, send_cant_do_msg,
    set_waiting_invoice_status, show_hold_invoice, update_order_event,
};

use mostro_core::error::CantDoReason;
use mostro_core::error::MostroError::{self, *};
use mostro_core::error::ServiceError;

use anyhow::{Error, Result};
use mostro_core::message::Message;
use mostro_core::order::{Kind, Order, Status};
use nostr::nips::nip59::UnwrappedGift;
use nostr_sdk::prelude::*;
use sqlx::{Pool, Sqlite};
use sqlx_crud::Crud;

async fn update_order_status(
    order: &mut Order,
    my_keys: &Keys,
    pool: &Pool<Sqlite>,
    request_id: Option<u64>,
) -> Result<(), MostroError> {
    // Get buyer pubkey
    let buyer_pubkey = order.get_buyer_pubkey().unwrap();
    // Set order status to waiting buyer invoice
    match set_waiting_invoice_status(order, buyer_pubkey, request_id).await {
        Ok(_) => {
            // Update order status
            match update_order_event(my_keys, Status::WaitingBuyerInvoice, &order).await {
                Ok(order_updated) => {
                    let _ = order_updated.update(pool).await;
                    return Ok(());
                }
                Err(_) => {
                    return Err(MostroInternalErr(ServiceError::UpdateOrderStatusError));
                }
            }
        }
        Err(_) => {
            return Err(MostroInternalErr(ServiceError::UpdateOrderStatusError));
        }
    }
}

async fn validate_invoice(msg: &Message, order: &Order) -> Result<Option<String>, MostroError> {
    // init payment request to None
    let mut payment_request = None;
    // if payment request is present
    if let Some(pr) = msg.get_inner_message_kind().get_payment_request() {
        // if invoice is valid
        if is_valid_invoice(
            pr.clone(),
            Some(order.amount as u64),
            Some(order.fee as u64),
        )
        .await
        .is_err()
        {
            return Err(MostroInternalErr(ServiceError::InvoiceInvalidError));
        }
        // if invoice is valid return it
        else {
            payment_request = Some(pr);
        }
    }
    Ok(payment_request)
}

pub async fn take_sell_action(
    msg: Message,
    event: &UnwrappedGift,
    my_keys: &Keys,
    pool: &Pool<Sqlite>,
) -> Result<(), MostroError> {
    // Extract order ID from the message, returning an error if not found
    // Safe unwrap as we verified the message
    let order_id = if let Some(order_id) = msg.get_inner_message_kind().id {
        order_id
    } else {
        return Err(MostroInternalErr(ServiceError::InvalidOrderId));
    };

    // Get request id
    let request_id = msg.get_inner_message_kind().request_id;

    let mut order = match Order::by_id(pool, order_id).await {
        Ok(Some(order)) => order,
        Ok(None) => {
            return Err(MostroInternalErr(ServiceError::InvalidOrderId));
        }
        Err(_) => {
            return Err(MostroInternalErr(ServiceError::DbAccessError));
        }
    };

    // Check if the order is a sell order and if its status is active
    if let Err(cause) = order.is_sell_order() {
        return Err(MostroCantDo(cause));
    };
    // Check if the order status is pending
    if let Err(cause) = order.check_status(Status::Pending) {
        return Err(MostroCantDo(cause));
    }

    // Validate that the order was sent from the correct maker
    if let Err(cause) = order.sent_from_maker(event.rumor.pubkey.to_hex()) {
        return Err(MostroCantDo(cause));
    }

    // Get seller pubkey
    let seller_pubkey = match order.get_seller_pubkey() {
        Some(pk) => pk,
        None => return Err(MostroInternalErr(ServiceError::InvalidPubkey)),
    };

    // Validate invoice and get payment request if present
    let payment_request = validate_invoice(&msg, &order).await?;

    // Get amount request if user requested one for range order - fiat amount will be used below
    if let Some(am) = get_fiat_amount_requested(&order, &msg) {
        order.fiat_amount = am;
    } else {
        return Err(MostroInternalErr(ServiceError::WrongAmountError));
    }

    // Add buyer pubkey to order
    order.buyer_pubkey = Some(event.rumor.pubkey.to_string());
    // Add buyer identity pubkey to order
    order.master_buyer_pubkey = Some(event.sender.to_string());
    // Add buyer trade index to order
    order.trade_index_buyer = msg.get_inner_message_kind().trade_index;
    // Timestamp take order time
    order.set_timestamp_now();

    // Check market price value in sats - if order was with market price then calculate it and send a DM to buyer
    if order.has_no_amount() {
        match get_market_amount_and_fee(order.fiat_amount, &order.fiat_code, order.premium).await {
            Ok(amount_fees) => {
                order.amount = amount_fees.0;
                order.fee = amount_fees.1
            }
            Err(_) => return Err(MostroInternalErr(ServiceError::WrongAmountError)),
        };
    }

    // If payment request is not present, update order status to waiting buyer invoice
    if payment_request.is_none() {
        match update_order_status(&mut order, my_keys, pool, request_id).await {
            Ok(_) => Ok(()),
            Err(_) => return Err(MostroInternalErr(ServiceError::UpdateOrderStatusError)),
        }
    }
    // If payment request is present, show hold invoice
    else {
        match show_hold_invoice(
            my_keys,
            payment_request,
            &event.rumor.pubkey,
            &seller_pubkey,
            order,
            request_id,
        )
        .await
        {
            Ok(_) => Ok(()),
            Err(_) => return Err(MostroInternalErr(ServiceError::HoldInvoiceError)),
        }
    }
}
