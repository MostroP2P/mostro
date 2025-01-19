use crate::util::{send_cant_do_msg, send_new_order_msg};

use anyhow::{Error, Result};
use mostro_core::message::{Action, Message};
use mostro_core::order::{Order, Status};
use nostr::nips::nip59::UnwrappedGift;
use nostr_sdk::prelude::*;
use sqlx::{Pool, Sqlite};
use sqlx_crud::Crud;
use tracing::error;

pub async fn trade_pubkey_action(
    msg: Message,
    event: &UnwrappedGift,
    pool: &Pool<Sqlite>,
) -> Result<()> {
    // Get request id
    let request_id = msg.get_inner_message_kind().request_id;

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
    // Send to user a DM with the error
    if order.status != Status::Pending.to_string() {
        send_cant_do_msg(
            request_id,
            Some(order.id),
            Some(CantDoReason::NotAllowedByStatus),
            &event.rumor.pubkey,
        )
        .await;

        return Ok(());
    }

    match (
        order.master_buyer_pubkey.as_ref(),
        order.master_seller_pubkey.as_ref(),
    ) {
        (Some(master_buyer_pubkey), _) if master_buyer_pubkey == &event.sender.to_string() => {
            order.buyer_pubkey = Some(event.rumor.pubkey.to_string());
        }
        (_, Some(master_seller_pubkey)) if master_seller_pubkey == &event.sender.to_string() => {
            order.seller_pubkey = Some(event.rumor.pubkey.to_string());
        }
        _ => return Err(Error::msg("Invalid pubkey")),
    };
    order.creator_pubkey = event.rumor.pubkey.to_string();

    // We a message to the seller
    send_new_order_msg(
        request_id,
        Some(order.id),
        Action::TradePubkey,
        None,
        &event.rumor.pubkey,
        None,
    )
    .await;

    order.update(pool).await?;

    Ok(())
}
