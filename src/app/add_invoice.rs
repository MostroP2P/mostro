use crate::lightning::invoice::is_valid_invoice;
use crate::util::{send_cant_do_msg, send_new_order_msg, show_hold_invoice, update_order_event};

use anyhow::Result;

use mostro_core::message::{Action, Content, Message};
use mostro_core::order::SmallOrder;
use mostro_core::order::{Kind, Order, Status};
use nostr_sdk::prelude::*;
use sqlx::{Pool, Sqlite};
use sqlx_crud::Crud;
use std::str::FromStr;
use tracing::error;

pub async fn add_invoice_action(
    msg: Message,
    event: &Event,
    my_keys: &Keys,
    pool: &Pool<Sqlite>,
) -> Result<()> {
    let order_msg = msg.get_inner_message_kind();
    // Safe unwrap as we verified the message
    let order_id = order_msg.id.unwrap();
    let mut order = match Order::by_id(pool, order_id).await? {
        Some(order) => order,
        None => {
            error!("Order Id {order_id} not found!");
            return Ok(());
        }
    };

    let order_status = match Status::from_str(&order.status) {
        Ok(s) => s,
        Err(e) => {
            error!("Order Id {order_id} wrong status: {e:?}");
            return Ok(());
        }
    };

    let order_kind = match Kind::from_str(&order.kind) {
        Ok(k) => k,
        Err(e) => {
            error!("Order Id {order_id} wrong kind: {e:?}");
            return Ok(());
        }
    };

    let buyer_pubkey = match order.buyer_pubkey.as_ref() {
        Some(pk) => PublicKey::from_str(pk)?,
        None => {
            error!("Buyer pubkey not found for order {}!", order.id);
            return Ok(());
        }
    };
    // Only the buyer can add an invoice
    if buyer_pubkey != event.pubkey {
        send_cant_do_msg(
            Some(order_id),
            Some("Not allowed".to_string()),
            &event.pubkey,
        )
        .await;
        return Ok(());
    }

    // Invoice variable
    let invoice: String;
    // If a buyer sent me a lightning invoice or a ln address we handle it
    if let Some(payment_request) = order_msg.get_payment_request() {
        invoice = {
            // Verify if invoice is valid
            match is_valid_invoice(
                payment_request.clone(),
                Some(order.amount as u64),
                Some(order.fee as u64),
            )
            .await
            {
                Ok(_) => payment_request,
                Err(e) => {
                    send_cant_do_msg(Some(order_id), Some(e.to_string()), &event.pubkey).await;
                    return Ok(());
                }
            }
        };
    } else {
        error!("Order Id {order_id} wrong get_payment_request");
        return Ok(());
    }
    // We save the invoice on db
    order.buyer_invoice = Some(invoice);
    // Buyer can add invoice orders with WaitingBuyerInvoice status
    match order_status {
        Status::WaitingBuyerInvoice => {}
        Status::SettledHoldInvoice => {
            order.payment_attempts = 0;
            order.update(pool).await?;
            send_new_order_msg(
                Some(order_id),
                Action::AddInvoice,
                Some(Content::TextMessage(format!(
                    "Order Id {order_id}: Invoice updated!"
                ))),
                &buyer_pubkey,
            )
            .await;
            return Ok(());
        }
        _ => {
            send_cant_do_msg(
                Some(order_id),
                Some(format!(
                    "You are not allowed to add an invoice because order Id {order_id} status is {}", order_status.to_string()
                )),
                &buyer_pubkey,
            )
            .await;
            return Ok(());
        }
    }
    let seller_pubkey = order.seller_pubkey.as_ref().cloned().unwrap();
    let seller_pubkey = PublicKey::from_str(&seller_pubkey)?;

    if order.preimage.is_some() {
        // We send this data related to the order to the parties
        let order_data = SmallOrder::new(
            Some(order.id),
            Some(order_kind),
            Some(Status::Active),
            order.amount,
            order.fiat_code.clone(),
            order.fiat_amount,
            order.payment_method.clone(),
            order.premium,
            order.buyer_pubkey.as_ref().cloned(),
            order.seller_pubkey.as_ref().cloned(),
            None,
            None,
            None,
        );
        // We publish a new replaceable kind nostr event with the status updated
        // and update on local database the status and new event id
        if let Ok(order_updated) = update_order_event(my_keys, Status::Active, &order).await {
            let _ = order_updated.update(pool).await;
        }

        // We send a confirmation message to seller
        send_new_order_msg(
            Some(order.id),
            Action::BuyerTookOrder,
            Some(Content::Order(order_data.clone())),
            &seller_pubkey,
        )
        .await;
        // We send a message to buyer saying seller paid
        send_new_order_msg(
            Some(order.id),
            Action::HoldInvoicePaymentAccepted,
            Some(Content::Order(order_data)),
            &buyer_pubkey,
        )
        .await;
    } else {
        show_hold_invoice(my_keys, None, &buyer_pubkey, &seller_pubkey, order).await?;
    }

    Ok(())
}
