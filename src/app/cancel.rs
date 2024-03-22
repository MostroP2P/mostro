use crate::db::{edit_buyer_pubkey_order, edit_seller_pubkey_order, update_order_to_initial_state};
use crate::lightning::LndConnector;
use crate::util::{send_cant_do_msg, send_dm, update_order_event};
use anyhow::Result;
use mostro_core::message::{Action, Message};
use mostro_core::order::{Kind as OrderKind, Order, Status};
use nostr_sdk::prelude::*;
use sqlx::{Pool, Sqlite};
use sqlx_crud::Crud;
use std::str::FromStr;
use tracing::{error, info};

pub async fn cancel_action(
    msg: Message,
    event: &Event,
    my_keys: &Keys,
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
    if order.status == Status::Pending.to_string() {
        let user_pubkey = event.pubkey.to_string();
        // Validates if this user is the order creator
        if user_pubkey != order.creator_pubkey {
            // We create a Message
            send_cant_do_msg(
                Some(order_id),
                Some("Not allowed!".to_string()),
                &event.pubkey,
            )
            .await;
        } else {
            // We publish a new replaceable kind nostr event with the status updated
            // and update on local database the status and new event id
            if let Ok(order_updated) = update_order_event(my_keys, Status::Canceled, &order).await {
                let _ = order_updated.update(pool).await;
            }
            // We create a Message for cancel
            let message = Message::new_order(Some(order.id), None, Action::Cancel, None);
            let message = message.as_json()?;
            send_dm(my_keys, &event.pubkey, message).await?;
        }

        return Ok(());
    }

    if order.kind == OrderKind::Sell.to_string()
        && order.status == Status::WaitingBuyerInvoice.to_string()
    {
        cancel_add_invoice(ln_client, &mut order, event, pool, my_keys).await?;
    }

    if order.kind == OrderKind::Buy.to_string()
        && order.status == Status::WaitingPayment.to_string()
    {
        cancel_pay_hold_invoice(ln_client, &mut order, event, pool, my_keys).await?;
    }

    if order.status == Status::Active.to_string()
        || order.status == Status::FiatSent.to_string()
        || order.status == Status::Dispute.to_string()
    {
        let user_pubkey = event.pubkey.to_string();
        let buyer_pubkey = order.buyer_pubkey.as_ref().unwrap();
        let seller_pubkey = order.seller_pubkey.as_ref().unwrap();
        let counterparty_pubkey: String;
        if buyer_pubkey == &user_pubkey {
            order.buyer_cooperativecancel = true;
            counterparty_pubkey = seller_pubkey.to_string();
        } else {
            order.seller_cooperativecancel = true;
            counterparty_pubkey = buyer_pubkey.to_string();
        }

        match order.cancel_initiator_pubkey {
            Some(ref initiator_pubkey) => {
                if initiator_pubkey == &user_pubkey {
                    // We create a Message
                    send_cant_do_msg(Some(order_id), None, &event.pubkey).await;
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
                    order.status = Status::CooperativelyCanceled.to_string();
                    // We publish a new replaceable kind nostr event with the status updated
                    // and update on local database the status and new event id
                    update_order_event(my_keys, Status::CooperativelyCanceled, &order).await?;
                    // We create a Message for an accepted cooperative cancel and send it to both parties
                    let message = Message::new_order(
                        Some(order.id),
                        None,
                        Action::CooperativeCancelAccepted,
                        None,
                    );
                    let message = message.as_json()?;
                    send_dm(my_keys, &event.pubkey, message.clone()).await?;
                    let counterparty_pubkey = XOnlyPublicKey::from_str(&counterparty_pubkey)?;
                    send_dm(my_keys, &counterparty_pubkey, message).await?;
                    info!("Cancel: Order Id {order_id} canceled cooperatively!");
                }
            }
            None => {
                order.cancel_initiator_pubkey = Some(user_pubkey.clone());
                // update db
                let order = order.update(pool).await?;
                // We create a Message to start a cooperative cancel and send it to both parties
                let message = Message::new_order(
                    Some(order.id),
                    None,
                    Action::CooperativeCancelInitiatedByYou,
                    None,
                );
                let message = message.as_json()?;
                send_dm(my_keys, &event.pubkey, message).await?;
                let message = Message::new_order(
                    Some(order.id),
                    None,
                    Action::CooperativeCancelInitiatedByPeer,
                    None,
                );
                let message = message.as_json()?;
                let counterparty_pubkey = XOnlyPublicKey::from_str(&counterparty_pubkey)?;
                send_dm(my_keys, &counterparty_pubkey, message).await?;
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
    my_keys: &Keys,
) -> Result<()> {
    if order.hash.is_some() {
        // We return funds to seller
        let hash = order.hash.as_ref().unwrap();
        ln_client.cancel_hold_invoice(hash).await?;
        info!("Order Id {}: Funds returned to seller", &order.id);
    }
    let user_pubkey = event.pubkey.to_string();
    let buyer_pubkey = order.buyer_pubkey.as_ref().unwrap();
    let seller_pubkey = order.seller_pubkey.as_ref().cloned().unwrap();
    let seller_pubkey = match XOnlyPublicKey::from_str(&seller_pubkey) {
        Ok(pk) => pk,
        Err(e) => {
            error!("Error parsing seller pubkey: {:#?}", e);
            return Ok(());
        }
    };
    if buyer_pubkey != &user_pubkey {
        // We create a Message
        send_cant_do_msg(Some(order.id), None, &event.pubkey).await;
        return Ok(());
    }

    if &order.creator_pubkey == buyer_pubkey {
        // We publish a new replaceable kind nostr event with the status updated
        // and update on local database the status and new event id
        update_order_event(my_keys, Status::CooperativelyCanceled, order).await?;
        // We create a Message for cancel
        let message = Message::new_order(Some(order.id), None, Action::Cancel, None);
        let message = message.as_json()?;
        send_dm(my_keys, &event.pubkey, message.clone()).await?;
        send_dm(my_keys, &seller_pubkey, message).await?;
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
        update_order_event(my_keys, Status::Pending, order).await?;
        info!(
            "{}: Canceled order Id {} republishing order",
            buyer_pubkey, order.id
        );
        Ok(())
    }
}

pub async fn cancel_pay_hold_invoice(
    ln_client: &mut LndConnector,
    order: &mut Order,
    event: &Event,
    pool: &Pool<Sqlite>,
    my_keys: &Keys,
) -> Result<()> {
    if order.hash.is_some() {
        // We return funds to seller
        let hash = order.hash.as_ref().unwrap();
        ln_client.cancel_hold_invoice(hash).await?;
        info!("Order Id {}: Funds returned to seller", &order.id);
    }
    let user_pubkey = event.pubkey.to_string();
    let buyer_pubkey = order.buyer_pubkey.as_ref().unwrap();
    let seller_pubkey = order.seller_pubkey.as_ref().unwrap();
    let seller_pubkey = match XOnlyPublicKey::from_str(seller_pubkey) {
        Ok(pk) => pk,
        Err(e) => {
            error!("Error parsing seller pubkey: {:#?}", e);
            return Ok(());
        }
    };
    if seller_pubkey.to_string() != user_pubkey {
        // We create a Message
        send_cant_do_msg(Some(order.id), None, &event.pubkey).await;
        return Ok(());
    }

    if order.creator_pubkey == seller_pubkey.to_string() {
        // We publish a new replaceable kind nostr event with the status updated
        // and update on local database the status and new event id
        update_order_event(my_keys, Status::Canceled, order).await?;
        // We create a Message for cancel
        let message = Message::new_order(Some(order.id), None, Action::Cancel, None);
        let message = message.as_json()?;
        send_dm(my_keys, &event.pubkey, message.clone()).await?;
        send_dm(my_keys, &seller_pubkey, message).await?;
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
        update_order_event(my_keys, Status::Pending, order).await?;
        info!(
            "{}: Canceled order Id {} republishing order",
            buyer_pubkey, order.id
        );
        Ok(())
    }
}
