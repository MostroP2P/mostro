use crate::db::update_order_to_initial_state;

use crate::lightning::LndConnector;
use crate::messages;
use crate::util::{send_dm, update_order_event};

use anyhow::Result;
use log::{error, info};
use mostro_core::order::Order;
use mostro_core::{Action, Content, Message, Status};
use nostr_sdk::prelude::*;
use sqlx::{Pool, Sqlite};
use sqlx_crud::Crud;

pub async fn cancel_action(
    msg: Message,
    event: &Event,
    my_keys: &Keys,
    client: &Client,
    pool: &Pool<Sqlite>,
    ln_client: &mut LndConnector,
) -> Result<()> {
    let order_id = msg.order_id.unwrap();
    let order = match Order::by_id(&pool, order_id).await? {
        Some(order) => order,
        None => {
            error!("Cancel: Order Id {order_id} not found!");
            return Ok(());
        }
    };
    if order.status == "Pending" {
        // Validates if this user is the order creator
        let user_pubkey = event.pubkey.to_bech32()?;
        if user_pubkey != order.creator_pubkey {
            let text_message = messages::cant_do();
            // We create a Message
            let message = Message::new(
                0,
                Some(order.id),
                Action::CantDo,
                Some(Content::TextMessage(text_message)),
            );
            let message = message.as_json()?;
            send_dm(&client, &my_keys, &event.pubkey, message).await?;
            return Ok(());
        }
        // We publish a new replaceable kind nostr event with the status updated
        // and update on local database the status and new event id
        update_order_event(&pool, &client, &my_keys, Status::Canceled, &order, None).await?;
        // We create a Message for cancel
        let message = Message::new(0, Some(order.id), Action::Cancel, None);
        let message = message.as_json()?;
        send_dm(&client, &my_keys, &event.pubkey, message).await?;
    } else if order.status == "WaitingBuyerInvoice" {
        // We return funds to seller
        let hash = order.hash.as_ref().unwrap();
        ln_client.cancel_hold_invoice(hash).await?;
        info!("Cancel: Order Id {}: Funds returned to seller", &order.id);
        let creator = event.pubkey.to_bech32()?;
        if &creator == order.buyer_pubkey.as_ref().unwrap() {
            // We publish a new replaceable kind nostr event with the status updated
            // and update on local database the status and new event id
            update_order_event(&pool, &client, &my_keys, Status::Canceled, &order, None).await?;
            // We create a Message for cancel
            let message = Message::new(0, Some(order.id), Action::Cancel, None);
            let message = message.as_json()?;
            send_dm(&client, &my_keys, &event.pubkey, message).await?;
        } else {
            // We re-publish the event with Pending status
            // and update on local database
            let mut amount = order.amount;
            let mut fee = order.fee;
            if order.price_from_api {
                amount = 0;
                fee = 0;
            }
            update_order_to_initial_state(&pool, order.id, amount, fee).await?;
            update_order_event(&pool, &client, &my_keys, Status::Pending, &order, None).await?;
            info!(
                "Buyer: {}: Canceled order Id {} republishing order",
                order.buyer_pubkey.as_ref().unwrap(),
                &order.id
            );
        }
    } else if order.status == "WaitingPayment" {
        // TODO
        unimplemented!()
    } else if order.status == "Active" || order.status == "FiatSent" || order.status == "Dispute" {
        // TODO
        unimplemented!()
    } else {
        // TODO
        unimplemented!()
    }
    Ok(())
}
