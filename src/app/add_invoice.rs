use crate::error::MostroError;
use crate::lightning::invoice::is_valid_invoice;
use crate::util::send_dm;

use anyhow::Result;
use log::error;
use mostro_core::order::Order;
use mostro_core::{order::SmallOrder, Action, Content, Message, Status};
use nostr_sdk::prelude::*;
use sqlx::{Pool, Sqlite};
use sqlx_crud::Crud;
use std::str::FromStr;

pub async fn add_invoice_action(
    msg: Message,
    event: &Event,
    my_keys: &Keys,
    client: &Client,
    pool: &Pool<Sqlite>,
) -> Result<()> {
    // Safe unwrap as we verified the message
    let order_id = msg.order_id.unwrap();
    let order = match Order::by_id(pool, order_id).await? {
        Some(order) => order,
        None => {
            error!("AddInvoice: Order Id {order_id} not found!");
            return Ok(());
        }
    };
    if order.kind != "Buy" {
        error!("AddInvoice: Order Id {order_id} wrong kind");
        return Ok(());
    }
    let buyer_pubkey = event.pubkey;
    let pr: String;
    // If a buyer sent me a lightning invoice we look on db an order with
    // that order id and save the buyer pubkey and invoice fields
    if let Some(payment_request) = msg.get_payment_request() {
        // Verify if invoice is valid
        match is_valid_invoice(&payment_request, Some(order.amount as u64)) {
            Ok(_) => {}
            Err(e) => match e {
                MostroError::ParsingInvoiceError
                | MostroError::InvoiceExpiredError
                | MostroError::MinExpirationTimeError
                | MostroError::WrongAmountError
                | MostroError::MinAmountError => {
                    // We create a Message
                    let message = Message::new(
                        0,
                        Some(order.id),
                        Action::CantDo,
                        Some(Content::TextMessage(e.to_string())),
                    );
                    let message = message.as_json()?;
                    send_dm(client, my_keys, &buyer_pubkey, message).await?;
                    error!("{e}");
                    return Ok(());
                }
                _ => {}
            },
        }
        pr = payment_request;
    } else {
        error!("AddInvoice: Order Id {order_id} wrong get_payment_request");
        return Ok(());
    }

    let order_status = match Status::from_str(&order.status) {
        Ok(s) => s,
        Err(e) => {
            error!("AddInvoice: Order Id {order_id} wrong status: {e:?}");
            return Ok(());
        }
    };
    // Buyer can take pending orders only
    match order_status {
        Status::WaitingBuyerInvoice => {}
        _ => {
            // We create a Message
            let message = Message::new(
                0,
                Some(order.id),
                Action::CantDo,
                Some(Content::TextMessage(format!(
                    "Order Id {order_id} status must be WaitingBuyerInvoice!"
                ))),
            );
            let message = message.as_json()?;
            send_dm(client, my_keys, &buyer_pubkey, message).await?;
            return Ok(());
        }
    }
    // We send this data related to the order to the parties
    let order_data = SmallOrder::new(
        order.id,
        order.amount,
        order.fiat_code.clone(),
        order.fiat_amount,
        pr,
        order.premium,
        order.buyer_pubkey.as_ref().cloned(),
        order.seller_pubkey.as_ref().cloned(),
    );
    // We send a confirmation message to seller
    let message = Message::new(
        0,
        Some(order.id),
        Action::BuyerTookOrder,
        Some(Content::SmallOrder(order_data.clone())),
    );
    let message = message.as_json().unwrap();
    let seller_pubkey = order.seller_pubkey.as_ref().cloned().unwrap();
    let seller_pubkey = XOnlyPublicKey::from_str(&seller_pubkey).unwrap();
    send_dm(client, my_keys, &seller_pubkey, message)
        .await
        .unwrap();
    // We send a message to buyer saying seller paid
    let message = Message::new(
        0,
        Some(order.id),
        Action::HoldInvoicePaymentAccepted,
        Some(Content::SmallOrder(order_data)),
    );
    let message = message.as_json().unwrap();
    send_dm(client, my_keys, &buyer_pubkey, message)
        .await
        .unwrap();

    // We publish a new replaceable kind nostr event with the status updated
    // and update on local database the status and new event id
    crate::util::update_order_event(pool, client, my_keys, Status::Active, &order, None)
        .await
        .unwrap();
    Ok(())
}
