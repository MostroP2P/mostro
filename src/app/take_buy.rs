use crate::db::edit_master_seller_pubkey_order;
use crate::util::{get_market_quote, send_dm, show_hold_invoice};

use anyhow::Result;
use log::error;
use mostro_core::order::Order;
use mostro_core::{Action, Content, Message, Status};
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
    // Safe unwrap as we verified the message
    let order_id = msg.order_id.unwrap();
    let mut order = match Order::by_id(pool, order_id).await? {
        Some(order) => order,
        None => {
            error!("TakeBuy: Order Id {order_id} not found!");
            return Ok(());
        }
    };
    // We check if the message have a pubkey
    if msg.pubkey.is_none() {
        // We create a Message
        let message = Message::new(0, Some(order.id), None, Action::CantDo, None);
        let message = message.as_json()?;
        send_dm(client, my_keys, &event.pubkey, message).await?;

        return Ok(());
    }

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
    let buyer_pubkey = match order.buyer_pubkey.as_ref() {
        Some(pk) => XOnlyPublicKey::from_bech32(pk)?,
        None => {
            error!("TakeBuy: Buyer pubkey not found for order {}!", order.id);
            return Ok(());
        }
    };
    if buyer_pubkey == event.pubkey {
        // We create a Message
        let message = Message::new(0, Some(order.id), None, Action::CantDo, None);
        let message = message.as_json().unwrap();
        send_dm(client, my_keys, &event.pubkey, message).await?;

        return Ok(());
    }
    // We update the master pubkey
    edit_master_seller_pubkey_order(pool, order.id, msg.pubkey).await?;
    let seller_pubkey = event.pubkey;
    // Seller can take pending orders only
    if order_status != Status::Pending {
        // We create a Message
        let message = Message::new(
            0,
            Some(order.id),
            None,
            Action::FiatSent,
            Some(Content::TextMessage(format!(
                "Order Id {order_id} was already taken!"
            ))),
        );
        let message = message.as_json().unwrap();
        send_dm(client, my_keys, &seller_pubkey, message).await?;

        return Ok(());
    }

    // Timestamp order take time
    if order.taken_at == 0 {
        crate::db::update_order_taken_at_time(pool, order.id, Timestamp::now().as_i64()).await?;
    }

    // Check market price value in sats - if order was with market price then calculate
    if order.amount == 0 {
        order.amount =
            get_market_quote(&order.fiat_amount, &order.fiat_code, &order.premium).await?;
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
