use crate::lightning::invoice::is_valid_invoice;
use crate::util::{
    get_fiat_amount_requested, get_market_amount_and_fee, send_cant_do_msg,
    set_waiting_invoice_status, show_hold_invoice, update_order_event,
};

use mostro_core::error::MostroError::{self, *};
use mostro_core::error::ServiceError;
use mostro_core::error::CantDoReason;

use anyhow::{Error, Result};
use mostro_core::message::Message;
use mostro_core::order::{Kind, Order, Status};
use nostr::nips::nip59::UnwrappedGift;
use nostr_sdk::prelude::*;
use sqlx::{Pool, Sqlite};
use sqlx_crud::Crud;
use tracing::error;

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
            return Err(MostroInternalErr(ServiceError::OrderNotFound));
        }
    };

    // Check if the order is a buy order and if its status is active
    if let Err(cause) = order.is_sell_order() {
        return Err(MostroInternalErr(cause));
    };
    // Check if the order status is pending
    if let Err(cause) = order.check_status(Status::Pending) {
        return Err(MostroInternalErr(cause));
    }

    // Validate that the order was sent from the correct maker
    if let Err(cause) = order.sent_from_maker(event.rumor.pubkey.to_hex()) {
        return Err(MostroInternalErr(cause));
    }

    let seller_pubkey = match order.get_seller_pubkey() {
        Some(pk) => pk,
        None => return Err(MostroInternalErr(ServiceError::InvalidPubkey),
    };

    let mut pr: Option<String> = None;
    // If a buyer sent me a lightning invoice we look on db an order with
    // that order id and save the buyer pubkey and invoice fields
    if let Some(payment_request) = msg.get_inner_message_kind().get_payment_request() {
        pr = {
            // Verify if invoice is valid
            match is_valid_invoice(
                payment_request.clone(),
                Some(order.amount as u64),
                Some(order.fee as u64),
            )
            .await
            {
                Ok(_) => Some(payment_request),
                Err(_) => return Err(MostroInternalErr(ServiceError::InvoiceInvalidError)),
            }
        };
    }

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
            Ok(amount_fees   ) => {order.amount = amount_fees.0; order.fee = amount_fees.1}
            Err(_) => return Err(MostroInternalErr(ServiceError::WrongAmountError)),
        };
    }

    if pr.is_none() {
        match set_waiting_invoice_status(&mut order, buyer_trade_pubkey, request_id).await {
            Ok(_) => {
                // Update order status
                if let Ok(order_updated) =
                    update_order_event(my_keys, Status::WaitingBuyerInvoice, &order).await
                {
                    let _ = order_updated.update(pool).await;
                    return Ok(());
                }
            }
            Err(e) => {
                error!("Error setting market order sats amount: {:#?}", e);
                return Ok(());
            }
        }
    } else {
        show_hold_invoice(
            my_keys,
            pr,
            &buyer_trade_pubkey,
            &seller_pubkey,
            order,
            request_id,
        )
        .await?;
    }
    Ok(())
}
