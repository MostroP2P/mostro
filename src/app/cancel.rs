use crate::db::edit_buyer_pubkey_order;
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
    let mut order = match Order::by_id(pool, order_id).await? {
        Some(order) => order,
        None => {
            error!("Cancel: Order Id {order_id} not found!");
            return Ok(());
        }
    };
    if order.status == "Pending" {
        let user_pubkey = event.pubkey.to_bech32()?;
        // Validates if this user is the order creator
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
            send_dm(client, my_keys, &event.pubkey, message).await?;
            return Ok(());
        }
        // We publish a new replaceable kind nostr event with the status updated
        // and update on local database the status and new event id
        update_order_event(pool, client, my_keys, Status::Canceled, &order, None).await?;
        // We create a Message for cancel
        let message = Message::new(0, Some(order.id), Action::Cancel, None);
        let message = message.as_json()?;
        send_dm(client, my_keys, &event.pubkey, message).await?;
    }

    if order.kind == "Sell" && order.status == "WaitingBuyerInvoice" {
        cancel_add_invoice(ln_client, &mut order, event, pool, client, my_keys).await?;
    }

    if order.status == "WaitingPayment" {
        // TODO
        unimplemented!()
    } else if order.status == "Active" || order.status == "FiatSent" || order.status == "Dispute" {
        // TODO
        unimplemented!()
    } else {
        // TODO
        Ok(())
    }
}

pub async fn cancel_add_invoice(
    ln_client: &mut LndConnector,
    order: &mut Order,
    event: &Event,
    pool: &Pool<Sqlite>,
    client: &Client,
    my_keys: &Keys,
) -> Result<()> {
    if order.hash.is_some() {
        // We return funds to seller
        let hash = order.hash.as_ref().unwrap();
        ln_client.cancel_hold_invoice(hash).await?;
        info!("Cancel: Order Id {}: Funds returned to seller", &order.id);
    }
    let user_pubkey = event.pubkey.to_bech32()?;
    let buyer_pubkey_bech32 = order.buyer_pubkey.as_ref().unwrap();
    let seller_pubkey = order.seller_pubkey.as_ref().cloned().unwrap();
    let seller_pubkey = XOnlyPublicKey::from_bech32(seller_pubkey)?;
    if buyer_pubkey_bech32 != &user_pubkey {
        let text_message = messages::cant_do();
        // We create a Message
        let message = Message::new(
            0,
            Some(order.id),
            Action::CantDo,
            Some(Content::TextMessage(text_message)),
        );
        let message = message.as_json()?;
        send_dm(client, my_keys, &event.pubkey, message).await?;

        return Ok(());
    }

    if &order.creator_pubkey == buyer_pubkey_bech32 {
        // We publish a new replaceable kind nostr event with the status updated
        // and update on local database the status and new event id
        update_order_event(pool, client, my_keys, Status::Canceled, order, None).await?;
        // We create a Message for cancel
        let message = Message::new(0, Some(order.id), Action::Cancel, None);
        let message = message.as_json()?;
        send_dm(client, my_keys, &event.pubkey, message.clone()).await?;
        send_dm(client, my_keys, &seller_pubkey, message).await?;
        Ok(())
    } else {
        // We re-publish the event with Pending status
        // and update on local database
        if order.price_from_api {
            order.amount = 0;
            order.fee = 0;
        }
        edit_buyer_pubkey_order(pool, order.id, None).await?;
        update_order_to_initial_state(pool, order.id, order.amount, order.fee).await?;
        update_order_event(pool, client, my_keys, Status::Pending, order, None).await?;
        info!(
            "Buyer: {}: Canceled order Id {} republishing order",
            order.buyer_pubkey.as_ref().unwrap(),
            &order.id
        );
        Ok(())
    }
}
