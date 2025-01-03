use crate::util::{
    get_fiat_amount_requested, get_market_amount_and_fee, send_cant_do_msg, show_hold_invoice,
};

use anyhow::{Error, Result};
use mostro_core::message::{CantDoReason, Message};
use mostro_core::order::{Kind, Order, Status};
use nostr::nips::nip59::UnwrappedGift;
use nostr_sdk::prelude::*;
use sqlx::{Pool, Sqlite};
use sqlx_crud::Crud;
use std::str::FromStr;
use tracing::error;

pub async fn take_buy_action(
    msg: Message,
    event: &UnwrappedGift,
    my_keys: &Keys,
    pool: &Pool<Sqlite>,
) -> Result<()> {
    // Safe unwrap as we verified the message
    let order_id = if let Some(order_id) = msg.get_inner_message_kind().id {
        order_id
    } else {
        return Err(Error::msg("No order id"));
    };

    let request_id = msg.get_inner_message_kind().request_id;

    let mut order = match Order::by_id(pool, order_id).await? {
        Some(order) => order,
        None => {
            error!("Order Id {order_id} not found!");
            return Ok(());
        }
    };

    // Maker can't take own order
    if order.kind != Kind::Buy.to_string() || order.creator_pubkey == event.rumor.pubkey.to_hex() {
        send_cant_do_msg(
            request_id,
            Some(order.id),
            Some(CantDoReason::InvalidPubkey),
            &event.rumor.pubkey,
        )
        .await;
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

    // We update the pubkey
    let seller_pubkey = event.rumor.pubkey;
    // Seller can take pending orders only
    match order_status {
        Status::Pending => {}
        _ => {
            send_cant_do_msg(
                request_id,
                Some(order.id),
                Some(CantDoReason::NotAllowedByStatus),
                &event.rumor.pubkey,
            )
            .await;

            return Ok(());
        }
    }

    // Get amount request if user requested one for range order - fiat amount will be used below
    if let Some(am) = get_fiat_amount_requested(&order, &msg) {
        order.fiat_amount = am;
    } else {
        send_cant_do_msg(
            request_id,
            Some(order.id),
            Some(CantDoReason::OutOfRangeFiatAmount),
            &event.rumor.pubkey,
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

    // Add seller identity pubkey to order
    order.master_seller_pubkey = Some(event.sender.to_string());
    // Add seller trade index to order
    order.trade_index_seller = msg.get_inner_message_kind().trade_index;
    // Timestamp order take time
    order.taken_at = Timestamp::now().as_u64() as i64;

    show_hold_invoice(
        my_keys,
        None,
        &buyer_pubkey,
        &seller_pubkey,
        order,
        request_id,
    )
    .await?;
    Ok(())
}
