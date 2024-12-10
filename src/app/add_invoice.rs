use crate::lightning::invoice::is_valid_invoice;
use crate::util::{send_cant_do_msg, send_new_order_msg, show_hold_invoice, update_order_event};

use anyhow::{Error, Result};

use mostro_core::message::{Action, Message, Payload};
use mostro_core::order::SmallOrder;
use mostro_core::order::{Kind, Order, Status};
use nostr::nips::nip59::UnwrappedGift;
use nostr_sdk::prelude::*;
use sqlx::{Pool, Sqlite};
use sqlx_crud::Crud;
use std::str::FromStr;
use tracing::error;

pub async fn add_invoice_action(
    msg: Message,
    event: &UnwrappedGift,
    my_keys: &Keys,
    pool: &Pool<Sqlite>,
) -> Result<()> {
    // Get the order message
    let order_msg = msg.get_inner_message_kind();
    // Get the request id
    let request_id = order_msg.request_id;
    // Get the order
    let mut order = if let Some(order_id) = order_msg.id {
        match Order::by_id(pool, order_id).await? {
            Some(order) => order,
            None => return Err(Error::msg("Order Id {order_id} not found!")),
        }
    } else {
        return Err(Error::msg("Missing message Id"));
    };

    let order_status = match Status::from_str(&order.status) {
        Ok(s) => s,
        Err(e) => {
            error!("Order Id {} wrong status: {e:?}", order.id);
            return Ok(());
        }
    };

    let order_kind = match Kind::from_str(&order.kind) {
        Ok(k) => k,
        Err(e) => {
            error!("Order Id {} wrong kind: {e:?}", order.id);
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
    if buyer_pubkey != event.sender {
        send_cant_do_msg(request_id, Some(order.id), None, &event.sender).await;
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
                Err(_) => {
                    send_new_order_msg(
                        request_id,
                        Some(order.id),
                        Action::IncorrectInvoiceAmount,
                        None,
                        &event.sender,
                        None,
                    )
                    .await;
                    return Ok(());
                }
            }
        };
    } else {
        error!("Order Id {} wrong get_payment_request", order.id);
        return Ok(());
    }
    // We save the invoice on db
    order.buyer_invoice = Some(invoice);
    // Buyer can add invoice orders with WaitingBuyerInvoice status
    match order_status {
        Status::WaitingBuyerInvoice => {}
        Status::SettledHoldInvoice => {
            order.payment_attempts = 0;
            order.clone().update(pool).await?;
            send_new_order_msg(
                request_id,
                Some(order.id),
                Action::InvoiceUpdated,
                None,
                &buyer_pubkey,
                None,
            )
            .await;
            return Ok(());
        }
        _ => {
            send_new_order_msg(
                request_id,
                Some(order.id),
                Action::NotAllowedByStatus,
                None,
                &event.sender,
                None,
            )
            .await;
            return Ok(());
        }
    }

    let seller_pubkey = match &order.seller_pubkey {
        Some(seller) => PublicKey::from_str(seller.as_str())?,
        _ => return Err(Error::msg("Missing pubkeys")),
    };

    if order.preimage.is_some() {
        // We send this data related to the order to the parties
        let order_data = SmallOrder::new(
            Some(order.id),
            Some(order_kind),
            Some(Status::Active),
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
            None,
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
            None,
            Some(order.id),
            Action::BuyerTookOrder,
            Some(Payload::Order(order_data.clone())),
            &seller_pubkey,
            None,
        )
        .await;
        // We send a message to buyer saying seller paid
        send_new_order_msg(
            request_id,
            Some(order.id),
            Action::HoldInvoicePaymentAccepted,
            Some(Payload::Order(order_data)),
            &buyer_pubkey,
            None,
        )
        .await;
    } else {
        show_hold_invoice(
            my_keys,
            None,
            &buyer_pubkey,
            &seller_pubkey,
            order,
            request_id,
        )
        .await?;
    }

    Ok(())
}
