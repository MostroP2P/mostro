use crate::util::{
    get_fiat_amount_requested, get_market_amount_and_fee, send_cant_do_msg, send_new_order_msg,
    show_hold_invoice,
};

use anyhow::{Error, Result};
use mostro_core::message::{Action, Content, Message};
use mostro_core::order::{Kind, Order, Status};
use nostr_sdk::prelude::*;
use sqlx::{Pool, Sqlite};
use sqlx_crud::Crud;
use std::str::FromStr;
use tracing::error;

pub async fn take_buy_action(
    msg: Message,
    event: &Event,
    my_keys: &Keys,
    pool: &Pool<Sqlite>,
) -> Result<()> {
    // Safe unwrap as we verified the message
    let order_id = if let Some(order_id) = msg.get_inner_message_kind().id {
        order_id
    } else {
        return Err(Error::msg("No order id"));
    };

    let mut order = match Order::by_id(pool, order_id).await? {
        Some(order) => order,
        None => {
            error!("Order Id {order_id} not found!");
            return Ok(());
        }
    };
    // Maker can't take own order
    if order.creator_pubkey == event.pubkey.to_hex() {
        send_cant_do_msg(Some(order.id), None, &event.pubkey).await;
        return Ok(());
    }
    // We check if the message have a pubkey
    if msg.get_inner_message_kind().pubkey.is_none() {
        send_cant_do_msg(Some(order.id), None, &event.pubkey).await;
        return Ok(());
    }

    if order.kind != Kind::Buy.to_string() {
        error!("Order Id {order_id} wrong kind");
        send_cant_do_msg(Some(order.id), None, &event.pubkey).await;
        return Ok(());
    }

    // Get amount request if user requested one for range order - fiat amount will be used below
    if let Some(am) = get_fiat_amount_requested(&order, &msg) {
        order.fiat_amount = am;
    } else {
        let error = format!(
            "Amount requested is not correct, probably out of range min {:#?} - max {:#?}",
            order.min_amount, order.max_amount
        );
        send_cant_do_msg(Some(order.id), Some(error), &event.pubkey).await;
        return Ok(());
    }

    let order_status = match Status::from_str(&order.status) {
        Ok(s) => s,
        Err(e) => {
            error!("Order Id {order_id} wrong status: {e:?}");
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
    if buyer_pubkey == event.pubkey {
        send_cant_do_msg(Some(order.id), None, &event.pubkey).await;
        return Ok(());
    }
    // We update the master pubkey
    order
        .master_seller_pubkey
        .clone_from(&msg.get_inner_message_kind().pubkey);

    let seller_pubkey = event.pubkey;
    // Seller can take pending orders only
    if order_status != Status::Pending {
        // We create a Message
        send_new_order_msg(
            Some(order.id),
            Action::FiatSent,
            Some(Content::TextMessage(format!(
                "Order Id {order_id} was already taken!"
            ))),
            &seller_pubkey,
        )
        .await;
        return Ok(());
    }

    // Check market price value in sats - if order was with market price then calculate
    if order.amount == 0 {
        let (new_sats_amount, fee) =
            get_market_amount_and_fee(order.fiat_amount, &order.fiat_code, order.premium).await?;
        // Update order with new sats value
        order.amount = new_sats_amount;
        order.fee = fee;
    }

    // Timestamp order take time
    order.taken_at = Timestamp::now().as_i64();

    show_hold_invoice(my_keys, None, &buyer_pubkey, &seller_pubkey, order).await?;
    Ok(())
}
