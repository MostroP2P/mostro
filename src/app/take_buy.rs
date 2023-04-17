use crate::util::{send_dm, set_market_order_sats_amount, show_hold_invoice};

use anyhow::Result;
use log::error;
use mostro_core::order::Order;
use mostro_core::{Message, Status};
use nostr_sdk::prelude::*;
use sqlx::{Pool, Sqlite};
use sqlx_crud::Crud;
use std::str::FromStr;

pub async fn take_buy_action(
    msg: Message,
    event: &Event,
    my_keys: &Keys,
    client: &Client,
    pool: &Pool<Sqlite>,
) -> Result<()> {
    let seller_pubkey = event.pubkey;
    // Safe unwrap as we verified the message
    let order_id = msg.order_id.unwrap();
    let mut order = match Order::by_id(pool, order_id).await? {
        Some(order) => order,
        None => {
            error!("TakeBuy: Order Id {order_id} not found!");
            return Ok(());
        }
    };
    if order.kind != "Buy" {
        error!("TakeBuy: Order Id {order_id} wrong kind");
        return Ok(());
    }

    let order_status = match Status::from_str(&order.status) {
        Ok(s) => s,
        Err(e) => {
            error!("TakeBuy: Order Id {order_id} wrong status: {e:?}");
            return Ok(());
        }
    };
    // Buyer can take pending orders only
    if order_status != Status::Pending {
        send_dm(
            client,
            my_keys,
            &seller_pubkey,
            format!("Order Id {order_id} was already taken!"),
        )
        .await?;
        return Ok(());
    }
    let buyer_pubkey = match order.buyer_pubkey.as_ref() {
        Some(pk) => XOnlyPublicKey::from_bech32(pk)?,
        None => {
            error!("TakeBuy: Buyer pubkey not found for order {}!", order.id);
            return Ok(());
        }
    };
    // Check market price value in sats - if order was with market price then calculate it and send a DM to buyer
    if order.amount == 0 {
        order.amount =
            set_market_order_sats_amount(&mut order, buyer_pubkey, my_keys, pool, client).await?;
    }

    show_hold_invoice(
        pool,
        client,
        my_keys,
        None,
        &buyer_pubkey,
        &seller_pubkey,
        &order,
    )
    .await?;
    Ok(())
}
