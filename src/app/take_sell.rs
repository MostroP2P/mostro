use crate::util::{
    get_fiat_amount_requested, get_market_amount_and_fee, get_order, set_waiting_invoice_status,
    show_hold_invoice, update_order_event, validate_invoice,
};

use mostro_core::error::MostroError::{self, *};
use mostro_core::error::{CantDoReason, ServiceError};

use crate::db::update_user_trade_index;
use anyhow::Result;
use mostro_core::message::Message;
use mostro_core::order::{Order, Status};
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
            match update_order_event(my_keys, Status::WaitingBuyerInvoice, order).await {
                Ok(order_updated) => {
                    let _ = order_updated.update(pool).await;
                    Ok(())
                }
                Err(_) => Err(MostroInternalErr(ServiceError::UpdateOrderStatusError)),
            }
        }
        Err(_) => Err(MostroInternalErr(ServiceError::UpdateOrderStatusError)),
    }
}

pub async fn take_sell_action(
    msg: Message,
    event: &UnwrappedGift,
    my_keys: &Keys,
    pool: &Pool<Sqlite>,
) -> Result<(), MostroError> {
    // Get order
    let mut order = get_order(&msg, pool).await?;

    // Get request id
    let request_id = msg.get_inner_message_kind().request_id;

    // Check if the order is a sell order and if its status is active
    if let Err(cause) = order.is_sell_order() {
        return Err(MostroCantDo(cause));
    };
    // Check if the order status is pending
    if let Err(cause) = order.check_status(Status::Pending) {
        return Err(MostroCantDo(cause));
    }

    // Validate that the order was sent from the correct maker
    order
        .not_sent_from_maker(event.rumor.pubkey)
        .map_err(MostroCantDo)?;

    // Get seller pubkey
    let seller_pubkey = order.get_seller_pubkey().map_err(MostroInternalErr)?;

    // Validate invoice and get payment request if present
    let payment_request = validate_invoice(&msg, &order).await?;

    // Get amount request if user requested one for range order - fiat amount will be used below
    if let Some(am) = get_fiat_amount_requested(&order, &msg) {
        order.fiat_amount = am;
    } else {
        return Err(MostroCantDo(CantDoReason::OutOfRangeSatsAmount));
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

    if let Err(e) = update_user_trade_index(
        pool,
        event.sender.to_string(),
        msg.get_inner_message_kind().trade_index.unwrap(),
    )
    .await
    {
        tracing::error!("Error updating user trade index: {}", e);
    }

    // If payment request is not present, update order status to waiting buyer invoice
    if payment_request.is_none() {
        update_order_status(&mut order, my_keys, pool, request_id).await?;
    }
    // If payment request is present, show hold invoice
    else {
        show_hold_invoice(
            my_keys,
            payment_request,
            &event.rumor.pubkey,
            &seller_pubkey,
            order,
            request_id,
        )
        .await?;
    }

    Ok(())
}
