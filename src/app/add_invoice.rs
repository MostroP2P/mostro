use crate::error::MostroError;
use crate::lightning::invoice::is_valid_invoice;
use crate::lnurl::ln_exists;
use crate::util::{send_dm, show_hold_invoice};

use anyhow::Result;
use lnurl::lightning_address::LightningAddress;
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
    client: &Client,
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
        Some(pk) => XOnlyPublicKey::from_str(pk)?,
        None => {
            error!("Buyer pubkey not found for order {}!", order.id);
            return Ok(());
        }
    };
    // Only the buyer can add an invoice
    if buyer_pubkey != event.pubkey {
        let message = Message::cant_do(Some(order.id), None, None);
        let message = message.as_json().unwrap();
        send_dm(client, my_keys, &event.pubkey, message).await?;

        return Ok(());
    }
    let pr: String;
    // If a buyer sent me a lightning invoice or a ln address we handle it
    if let Some(payment_request) = order_msg.get_payment_request() {
        let invoice = {
            let ln_addr = LightningAddress::from_str(&payment_request);
            if ln_addr.is_ok() && ln_exists(&payment_request).await? {
                payment_request
            } else {
                // Verify if invoice is valid
                match is_valid_invoice(
                    &payment_request,
                    Some(order.amount as u64),
                    Some(order.fee as u64),
                ) {
                    Ok(_) => payment_request,
                    Err(e) => match e {
                        MostroError::ParsingInvoiceError
                        | MostroError::InvoiceExpiredError
                        | MostroError::MinExpirationTimeError
                        | MostroError::WrongAmountError
                        | MostroError::MinAmountError => {
                            let message = Message::cant_do(
                                Some(order.id),
                                None,
                                Some(Content::TextMessage(e.to_string())),
                            );
                            let message = message.as_json()?;
                            send_dm(client, my_keys, &event.pubkey, message).await?;
                            error!("{e}");
                            return Ok(());
                        }
                        _ => {
                            return Ok(());
                        }
                    },
                }
            }
        };
        pr = invoice;
    } else {
        error!("Order Id {order_id} wrong get_payment_request");
        return Ok(());
    }
    // We save the invoice on db
    order.buyer_invoice = Some(pr.clone());
    // Buyer can add invoice orders with WaitingBuyerInvoice status
    match order_status {
        Status::WaitingBuyerInvoice => {}
        Status::SettledHoldInvoice => {
            order.update(pool).await?;
            let message = Message::new_order(
                Some(order_id),
                None,
                Action::AddInvoice,
                Some(Content::TextMessage(format!(
                    "Order Id {order_id}: Invoice updated!"
                ))),
            );
            let message = message.as_json()?;
            send_dm(client, my_keys, &buyer_pubkey, message).await?;
            return Ok(());
        }
        _ => {
            let message = Message::cant_do(
                Some(order.id),
                None,
                Some(Content::TextMessage(format!(
                    "Order Id {order_id} status must be WaitingBuyerInvoice!"
                ))),
            );
            let message = message.as_json()?;
            send_dm(client, my_keys, &buyer_pubkey, message).await?;
            return Ok(());
        }
    }
    let seller_pubkey = order.seller_pubkey.as_ref().cloned().unwrap();
    let seller_pubkey = XOnlyPublicKey::from_str(&seller_pubkey)?;

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
        );
        // We publish a new replaceable kind nostr event with the status updated
        // and update on local database the status and new event id
        crate::util::update_order_event(pool, client, my_keys, Status::Active, &order)
            .await
            .unwrap();

        // We send a confirmation message to seller
        let message = Message::new_order(
            Some(order.id),
            None,
            Action::BuyerTookOrder,
            Some(Content::Order(order_data.clone())),
        );

        send_dm(client, my_keys, &seller_pubkey, message.as_json()?).await?;
        // We send a message to buyer saying seller paid
        let message = Message::new_order(
            Some(order.id),
            None,
            Action::HoldInvoicePaymentAccepted,
            Some(Content::Order(order_data)),
        );

        send_dm(client, my_keys, &buyer_pubkey, message.as_json()?)
            .await
            .unwrap();
    } else {
        show_hold_invoice(
            pool,
            client,
            my_keys,
            None,
            &buyer_pubkey,
            &seller_pubkey,
            order,
        )
        .await?;
    }

    Ok(())
}
