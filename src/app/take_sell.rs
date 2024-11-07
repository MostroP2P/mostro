use crate::lightning::invoice::is_valid_invoice;
use crate::util::{
    get_fiat_amount_requested, get_market_amount_and_fee, send_cant_do_msg, send_new_order_msg,
    set_waiting_invoice_status, show_hold_invoice, update_order_event,
};

use anyhow::{Error, Result};
use mostro_core::message::{Action, Message};
use mostro_core::order::{Kind, Order, Status};
use nostr::nips::nip59::UnwrappedGift;
use nostr_sdk::prelude::*;
use sqlx::{Pool, Sqlite};
use sqlx_crud::Crud;
use std::str::FromStr;
use tracing::error;

pub async fn take_sell_action(
    msg: Message,
    event: &UnwrappedGift,
    my_keys: &Keys,
    pool: &Pool<Sqlite>,
) -> Result<()> {
    // Get request id
    let request_id = msg.get_inner_message_kind().request_id;

    // Safe unwrap as we verified the message
    let order_id = if let Some(order_id) = msg.get_inner_message_kind().id {
        order_id
    } else {
        return Err(Error::msg("No order id"));
    };

    let mut order = match Order::by_id(pool, order_id).await? {
        Some(order) => order,
        None => {
            return Ok(());
        }
    };

    // Maker can't take own order
    if order.creator_pubkey == event.sender.to_hex() {
        send_cant_do_msg(request_id, Some(order.id), None, &event.sender).await;
        return Ok(());
    }

    if order.kind != Kind::Sell.to_string() {
        return Ok(());
    }

    let buyer_pubkey = event.sender;

    let seller_pubkey = match &order.seller_pubkey {
        Some(seller) => PublicKey::from_str(seller.as_str())?,
        _ => return Err(Error::msg("Missing seller pubkeys")),
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
                Err(e) => {
                    send_cant_do_msg(
                        request_id,
                        Some(order.id),
                        Some(e.to_string()),
                        &event.sender,
                    )
                    .await;
                    error!("{e}");
                    return Ok(());
                }
            }
        };
    }

    let order_status = match Status::from_str(&order.status) {
        Ok(s) => s,
        Err(e) => {
            error!("Order Id {order_id} wrong status: {e:?}");
            return Ok(());
        }
    };

    // Buyer can take Pending orders only
    match order_status {
        Status::Pending => {}
        _ => {
            send_new_order_msg(
                request_id,
                Some(order.id),
                Action::NotAllowedByStatus,
                None,
                &buyer_pubkey,
            )
            .await;
            return Ok(());
        }
    }

    // Get amount request if user requested one for range order - fiat amount will be used below
    if let Some(am) = get_fiat_amount_requested(&order, &msg) {
        order.fiat_amount = am;
    } else {
        send_new_order_msg(
            request_id,
            Some(order.id),
            Action::OutOfRangeFiatAmount,
            None,
            &event.sender,
        )
        .await;
        return Ok(());
    }

    // Add buyer pubkey to order
    order.buyer_pubkey = Some(buyer_pubkey.to_string());
    // Timestamp take order time
    order.taken_at = Timestamp::now().as_u64() as i64;

    // Check market price value in sats - if order was with market price then calculate it and send a DM to buyer
    if order.amount == 0 {
        let (new_sats_amount, fee) =
            get_market_amount_and_fee(order.fiat_amount, &order.fiat_code, order.premium).await?;
        // Update order with new sats value
        order.amount = new_sats_amount;
        order.fee = fee;
    }

    if pr.is_none() {
        match set_waiting_invoice_status(&mut order, buyer_pubkey, request_id).await {
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
            &buyer_pubkey,
            &seller_pubkey,
            order,
            request_id,
        )
        .await?;
    }
    Ok(())
}
