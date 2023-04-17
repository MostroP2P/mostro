use crate::error::MostroError;
use crate::lightning::invoice::is_valid_invoice;
use crate::util::{send_dm, set_market_order_sats_amount, show_hold_invoice};

use anyhow::Result;
use log::error;
use mostro_core::order::Order;
use mostro_core::{Message, Status};
use nostr_sdk::prelude::*;
use sqlx::{Pool, Sqlite};
use sqlx_crud::Crud;
use std::str::FromStr;

pub async fn take_sell_action(
    msg: Message,
    event: &Event,
    my_keys: &Keys,
    client: &Client,
    pool: &Pool<Sqlite>,
) -> Result<()> {
    // Safe unwrap as we verified the message
    let order_id = msg.order_id.unwrap();

    let mut order = match Order::by_id(&pool, order_id).await? {
        Some(order) => order,
        None => {
            error!("TakeSell: Order Id {order_id} not found!");
            return Ok(());
        }
    };
    if order.kind != "Sell" {
        error!("TakeSell: Order Id {order_id} wrong kind");
        return Ok(());
    }
    let buyer_pubkey = event.pubkey;
    let pr: Option<String>;
    // If a buyer sent me a lightning invoice we look on db an order with
    // that order id and save the buyer pubkey and invoice fields
    if let Some(payment_request) = msg.get_payment_request() {
        let order_amount = if order.amount == 0 {
            None
        } else {
            Some(order.amount as u64)
        };

        // Verify if invoice is valid
        match is_valid_invoice(&payment_request, order_amount) {
            Ok(_) => {}
            Err(e) => match e {
                MostroError::ParsingInvoiceError
                | MostroError::InvoiceExpiredError
                | MostroError::MinExpirationTimeError
                | MostroError::WrongAmountError
                | MostroError::MinAmountError => {
                    send_dm(&client, &my_keys, &buyer_pubkey, e.to_string()).await?;
                    error!("{e}");
                    return Ok(());
                }
                _ => {}
            },
        }
        pr = Some(payment_request);
    } else {
        pr = None;
    }

    let order_status = match Status::from_str(&order.status) {
        Ok(s) => s,
        Err(e) => {
            error!("TakeSell: Order Id {order_id} wrong status: {e:?}");
            return Ok(());
        }
    };
    // Buyer can take pending orders only
    match order_status {
        Status::Pending | Status::WaitingBuyerInvoice => {}
        _ => {
            send_dm(
                &client,
                &my_keys,
                &buyer_pubkey,
                format!("Order Id {order_id} was already taken!"),
            )
            .await?;
            return Ok(());
        }
    }
    let seller_pubkey = match order.seller_pubkey.as_ref() {
        Some(pk) => XOnlyPublicKey::from_bech32(pk)?,
        None => {
            error!("TakeSell: Seller pubkey not found for order {}!", order.id);
            return Ok(());
        }
    };

    // Check market price value in sats - if order was with market price then calculate it and send a DM to buyer
    if order.amount == 0 {
        order.amount =
            set_market_order_sats_amount(&mut order, buyer_pubkey, &my_keys, &pool, &client)
                .await?;
    } else {
        show_hold_invoice(
            &pool,
            &client,
            &my_keys,
            pr,
            &buyer_pubkey,
            &seller_pubkey,
            &order,
        )
        .await?;
    }

    Ok(())
}
