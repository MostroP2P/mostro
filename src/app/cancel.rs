use crate::db::{
    edit_buyer_pubkey_order, edit_seller_pubkey_order, init_cancel_order,
    update_order_to_initial_state,
};
use crate::lightning::LndConnector;
use crate::util::{send_dm, update_order_event};
use anyhow::Result;
use log::{error, info};
use mostro_core::message::{Action, Message};
use mostro_core::order::{Order, Status};
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
    let order_id = msg.get_inner_message_kind().id.unwrap();
    let mut order = match Order::by_id(pool, order_id).await? {
        Some(order) => order,
        None => {
            error!("Order Id {order_id} not found!");
            return Ok(());
        }
    };
    if order.status == "Pending" {
        let user_pubkey = event.pubkey.to_bech32()?;
        // Validates if this user is the order creator
        if user_pubkey != order.creator_pubkey {
            // We create a Message
            let message = Message::cant_do(Some(order.id), None, None);
            let message = message.as_json()?;
            send_dm(client, my_keys, &event.pubkey, message).await?;
        } else {
            // We publish a new replaceable kind nostr event with the status updated
            // and update on local database the status and new event id
            update_order_event(pool, client, my_keys, Status::Canceled, &order, None).await?;
            // We create a Message for cancel
            let message = Message::new_order(Some(order.id), None, Action::Cancel, None);
            let message = message.as_json()?;
            send_dm(client, my_keys, &event.pubkey, message).await?;
        }

        return Ok(());
    }

    if order.kind == "Sell" && order.status == "WaitingBuyerInvoice" {
        cancel_add_invoice(ln_client, &mut order, event, pool, client, my_keys).await?;
    }

    if order.kind == "Buy" && order.status == "WaitingPayment" {
        cancel_pay_hold_invoice(ln_client, &mut order, event, pool, client, my_keys).await?;
    }

    if order.status == "Active" || order.status == "FiatSent" || order.status == "Dispute" {
        let user_pubkey = event.pubkey.to_bech32()?;
        let buyer_pubkey_bech32 = order.buyer_pubkey.as_ref().unwrap();
        let seller_pubkey_bech32 = order.seller_pubkey.as_ref().unwrap();
        let counterparty_pubkey: String;
        if buyer_pubkey_bech32 == &user_pubkey {
            order.buyer_cooperativecancel = true;
            counterparty_pubkey = seller_pubkey_bech32.to_string();
        } else {
            order.seller_cooperativecancel = true;
            counterparty_pubkey = buyer_pubkey_bech32.to_string();
        }

        match order.cancel_initiator_pubkey {
            Some(ref initiator_pubkey) => {
                if initiator_pubkey == &user_pubkey {
                    // We create a Message
                    let message = Message::cant_do(Some(order.id), None, None);
                    let message = message.as_json()?;
                    send_dm(client, my_keys, &event.pubkey, message).await?;

                    return Ok(());
                } else {
                    if order.hash.is_some() {
                        // We return funds to seller
                        let hash = order.hash.as_ref().unwrap();
                        ln_client.cancel_hold_invoice(hash).await?;
                        info!(
                            "Cooperative cancel: Order Id {}: Funds returned to seller",
                            &order.id
                        );
                    }
                    init_cancel_order(pool, &order).await?;
                    order.status = "CooperativelyCanceled".to_string();
                    // We publish a new replaceable kind nostr event with the status updated
                    // and update on local database the status and new event id
                    update_order_event(
                        pool,
                        client,
                        my_keys,
                        Status::CooperativelyCanceled,
                        &order,
                        None,
                    )
                    .await?;
                    // We create a Message for an accepted cooperative cancel and send it to both parties
                    let message = Message::new_order(
                        Some(order.id),
                        None,
                        Action::CooperativeCancelAccepted,
                        None,
                    );
                    let message = message.as_json()?;
                    send_dm(client, my_keys, &event.pubkey, message.clone()).await?;
                    let counterparty_pubkey = XOnlyPublicKey::from_bech32(counterparty_pubkey)?;
                    send_dm(client, my_keys, &counterparty_pubkey, message).await?;
                    info!("Cancel: Order Id {order_id} canceled cooperatively!");
                }
            }
            None => {
                order.cancel_initiator_pubkey = Some(user_pubkey.clone());
                // update db
                init_cancel_order(pool, &order).await?;
                // We create a Message to start a cooperative cancel and send it to both parties
                let message = Message::new_order(
                    Some(order.id),
                    None,
                    Action::CooperativeCancelInitiatedByYou,
                    None,
                );
                let message = message.as_json()?;
                send_dm(client, my_keys, &event.pubkey, message).await?;
                let message = Message::new_order(
                    Some(order.id),
                    None,
                    Action::CooperativeCancelInitiatedByPeer,
                    None,
                );
                let message = message.as_json()?;
                let counterparty_pubkey = XOnlyPublicKey::from_bech32(counterparty_pubkey)?;
                send_dm(client, my_keys, &counterparty_pubkey, message).await?;
            }
        }
    }
    Ok(())
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
        info!("Order Id {}: Funds returned to seller", &order.id);
    }
    let user_pubkey = event.pubkey.to_bech32()?;
    let buyer_pubkey_bech32 = order.buyer_pubkey.as_ref().unwrap();
    let seller_pubkey = order.seller_pubkey.as_ref().cloned().unwrap();
    let seller_pubkey = XOnlyPublicKey::from_bech32(seller_pubkey)?;
    if buyer_pubkey_bech32 != &user_pubkey {
        // We create a Message
        let message = Message::cant_do(Some(order.id), None, None);
        let message = message.as_json()?;
        send_dm(client, my_keys, &event.pubkey, message).await?;

        return Ok(());
    }

    if &order.creator_pubkey == buyer_pubkey_bech32 {
        // We publish a new replaceable kind nostr event with the status updated
        // and update on local database the status and new event id
        update_order_event(
            pool,
            client,
            my_keys,
            Status::CooperativelyCanceled,
            order,
            None,
        )
        .await?;
        // We create a Message for cancel
        let message = Message::new_order(Some(order.id), None, Action::Cancel, None);
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
            "{}: Canceled order Id {} republishing order",
            buyer_pubkey_bech32, order.id
        );
        Ok(())
    }
}

pub async fn cancel_pay_hold_invoice(
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
        info!("Order Id {}: Funds returned to seller", &order.id);
    }
    let user_pubkey = event.pubkey.to_bech32()?;
    let buyer_pubkey_bech32 = order.buyer_pubkey.as_ref().unwrap();
    let seller_pubkey_bech32 = order.seller_pubkey.as_ref().unwrap();
    let seller_pubkey = XOnlyPublicKey::from_bech32(seller_pubkey_bech32)?;
    if seller_pubkey_bech32 != &user_pubkey {
        // We create a Message
        let message = Message::cant_do(Some(order.id), None, None);
        let message = message.as_json()?;
        send_dm(client, my_keys, &event.pubkey, message).await?;

        return Ok(());
    }

    if &order.creator_pubkey == seller_pubkey_bech32 {
        // We publish a new replaceable kind nostr event with the status updated
        // and update on local database the status and new event id
        update_order_event(pool, client, my_keys, Status::Canceled, order, None).await?;
        // We create a Message for cancel
        let message = Message::new_order(Some(order.id), None, Action::Cancel, None);
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
        edit_seller_pubkey_order(pool, order.id, None).await?;
        update_order_to_initial_state(pool, order.id, order.amount, order.fee).await?;
        update_order_event(pool, client, my_keys, Status::Pending, order, None).await?;
        info!(
            "{}: Canceled order Id {} republishing order",
            buyer_pubkey_bech32, order.id
        );
        Ok(())
    }
}
