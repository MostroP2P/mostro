use crate::error::MostroError;
use crate::lightning::invoice::is_valid_invoice;
use crate::lnurl::ln_exists;
use crate::util::{
    get_market_amount_and_fee, send_dm, set_waiting_invoice_status, show_hold_invoice,
};

use anyhow::Result;
use lnurl::lightning_address::LightningAddress;
use mostro_core::message::{Content, Message};
use mostro_core::order::{Order, Status};
use nostr_sdk::prelude::*;
use sqlx::{Pool, Sqlite};
use sqlx_crud::Crud;
use std::str::FromStr;
use std::thread;
use tracing::error;

pub async fn take_sell_action(
    msg: Message,
    event: &Event,
    my_keys: &Keys,
    client: &Client,
    pool: &Pool<Sqlite>,
) -> Result<()> {
    // Safe unwrap as we verified the message
    let order_id = msg.get_inner_message_kind().id.unwrap();

    let mut order = match Order::by_id(pool, order_id).await? {
        Some(order) => order,
        None => {
            return Ok(());
        }
    };
    if order.kind != "Sell" {
        return Ok(());
    }
    // We check if the message have a pubkey
    if msg.get_inner_message_kind().pubkey.is_none() {
        let message = Message::cant_do(Some(order.id), None, None);
        send_dm(client, my_keys, &event.pubkey, message.as_json()?).await?;

        return Ok(());
    }
    let buyer_pubkey = event.pubkey;
    let seller_pubkey = match order.seller_pubkey.as_ref() {
        Some(pk) => XOnlyPublicKey::from_str(pk)?,
        None => {
            error!("Seller pubkey not found for order {}!", order.id);
            return Ok(());
        }
    };
    let pr: Option<String>;
    // If a buyer sent me a lightning invoice we look on db an order with
    // that order id and save the buyer pubkey and invoice fields
    if let Some(payment_request) = msg.get_inner_message_kind().get_payment_request() {
        let order_amount = if order.amount == 0 {
            None
        } else {
            Some(order.amount as u64)
        };
        let payment_request = {
            let ln_addr = LightningAddress::from_str(&payment_request);
            if ln_addr.is_ok() && ln_exists(&payment_request).await? {
                payment_request
            } else {
                // Verify if invoice is valid
                match is_valid_invoice(&payment_request, order_amount, Some(order.fee as u64)) {
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
                            send_dm(client, my_keys, &buyer_pubkey, message.as_json()?).await?;
                            error!("{e}");
                            return Ok(());
                        }
                        _ => {
                            let message = Message::cant_do(Some(order.id), None, None);
                            send_dm(client, my_keys, &buyer_pubkey, message.as_json()?).await?;
                            error!("{e}");
                            return Ok(());
                        }
                    },
                }
            }
        };
        pr = Some(payment_request);
    } else {
        pr = None;
    }

    let order_status = match Status::from_str(&order.status) {
        Ok(s) => s,
        Err(e) => {
            error!("Order Id {order_id} wrong status: {e:?}");
            return Ok(());
        }
    };
    // Buyer can take Pending or WaitingBuyerInvoice orders only
    match order_status {
        Status::Pending | Status::WaitingBuyerInvoice => {}
        _ => {
            send_dm(
                client,
                my_keys,
                &buyer_pubkey,
                format!("Order Id {order_id} was already taken!"),
            )
            .await?;
            return Ok(());
        }
    }

    // We update the master pubkey
    order.master_buyer_pubkey = msg.get_inner_message_kind().pubkey.clone();
    // Add buyer pubkey to order
    order.buyer_pubkey = Some(buyer_pubkey.to_string());
    // Timestamp take order time
    order.taken_at = Timestamp::now().as_i64();
    let order_id = order.id;
    let mut order = order.update(pool).await?;

    // Check market price value in sats - if order was with market price then calculate it and send a DM to buyer
    if order.amount == 0 {
        let (new_sats_amount, fee) =
            get_market_amount_and_fee(order.fiat_amount, &order.fiat_code, order.premium).await?;
        // Update order with new sats value
        order.amount = new_sats_amount;
        order.fee = fee;
        let mut order = order.update(pool).await?;
        thread::sleep(std::time::Duration::from_secs(1));
        if pr.is_none() {
            match set_waiting_invoice_status(&mut order, buyer_pubkey, my_keys, pool, client).await
            {
                Ok(_) => {}
                Err(e) => {
                    error!("Error setting market order sats amount: {:#?}", e);
                    return Ok(());
                }
            }
        } else {
            show_hold_invoice(
                pool,
                client,
                my_keys,
                pr,
                &buyer_pubkey,
                &seller_pubkey,
                order_id,
            )
            .await?;
        }
    } else if pr.is_none() {
        match set_waiting_invoice_status(&mut order, buyer_pubkey, my_keys, pool, client).await {
            Ok(_) => {}
            Err(e) => {
                error!("Error setting market order sats amount: {:#?}", e);
                return Ok(());
            }
        }
    } else {
        show_hold_invoice(
            pool,
            client,
            my_keys,
            pr,
            &buyer_pubkey,
            &seller_pubkey,
            order_id,
        )
        .await?;
    }

    Ok(())
}
