use std::str::FromStr;

use crate::db::find_dispute_by_order_id;
use crate::nip33::new_event;
use crate::util::{send_cant_do_msg, send_new_order_msg};
use crate::NOSTR_CLIENT;

use anyhow::{Error, Result};
use mostro_core::dispute::Dispute;
use mostro_core::message::{Action, Message};
use mostro_core::order::{Order, Status};
use nostr_sdk::prelude::*;
use sqlx::{Pool, Sqlite};
use sqlx_crud::traits::Crud;
use tracing::{error, info};

pub async fn dispute_action(
    msg: Message,
    event: &Event,
    my_keys: &Keys,
    pool: &Pool<Sqlite>,
) -> Result<()> {
    let order_id = if let Some(order_id) = msg.get_inner_message_kind().id {
        order_id
    } else {
        return Err(Error::msg("No order id"));
    };

    // Check dispute for this order id is yet present.
    if find_dispute_by_order_id(pool, order_id).await.is_ok() {
        error!("Dispute yet opened for this order id: {order_id}");
        return Ok(());
    }

    let mut order = match Order::by_id(pool, order_id).await? {
        Some(order) => {
            if matches!(
                Status::from_str(&order.status).unwrap(),
                Status::Active | Status::FiatSent
            ) {
                order
            } else {
                send_cant_do_msg(
                    Some(order.id),
                    Some("Not allowed".to_string()),
                    &event.pubkey,
                )
                .await;
                return Ok(());
            }
        }

        None => {
            error!("Order Id {order_id} not found!");
            return Ok(());
        }
    };

    let (seller, buyer) = match (&order.seller_pubkey, &order.buyer_pubkey) {
        (Some(seller), Some(buyer)) => (seller.to_owned(), buyer.to_owned()),
        (None, _) => return Err(Error::msg("Missing seller pubkey")),
        (_, None) => return Err(Error::msg("Missing buyer pubkey")),
    };

    let message_sender = event.pubkey.to_string();
    // Get counterpart pubkey
    let mut counterpart: String = String::new();
    let mut buyer_dispute: bool = false;
    let mut seller_dispute: bool = false;

    // Find the counterpart public key
    if message_sender == buyer {
        counterpart = seller;
        buyer_dispute = true;
    } else if message_sender == seller {
        counterpart = buyer;
        seller_dispute = true;
    };

    // Add a check in case of no counterpart found
    if counterpart.is_empty() {
        // We create a Message
        send_cant_do_msg(Some(order.id), None, &event.pubkey).await;
        return Ok(());
    };

    let mut update_seller_dispute = false;
    let mut update_buyer_dispute = false;
    if seller_dispute && !order.seller_dispute {
        update_seller_dispute = true;
        order.seller_dispute = update_seller_dispute;
    } else if buyer_dispute && !order.buyer_dispute {
        update_buyer_dispute = true;
        order.buyer_dispute = update_buyer_dispute;
    };
    order.status = Status::Dispute.to_string();

    // Update the database with dispute information
    // Save the dispute to DB
    if !update_buyer_dispute && !update_seller_dispute {
        return Ok(());
    } else {
        // Need to update dispute status
        order.update(pool).await?;
    }
    let dispute = Dispute::new(order_id);
    // Use CRUD create method
    let dispute = dispute.create(pool).await?;

    // We create a Message for the initiator
    let initiator_pubkey = match PublicKey::from_str(&message_sender) {
        Ok(pk) => pk,
        Err(e) => {
            error!("Error parsing initiator pubkey: {:#?}", e);
            return Ok(());
        }
    };
    send_new_order_msg(
        Some(order_id),
        Action::DisputeInitiatedByYou,
        None,
        &initiator_pubkey,
    )
    .await;

    // We create a Message for the counterpart
    let counterpart_pubkey = match PublicKey::from_str(&counterpart) {
        Ok(pk) => pk,
        Err(e) => {
            error!("Error parsing counterpart pubkey: {:#?}", e);
            return Ok(());
        }
    };
    send_new_order_msg(
        Some(order_id),
        Action::DisputeInitiatedByPeer,
        None,
        &counterpart_pubkey,
    )
    .await;

    // We create a tag to show status of the dispute
    let tags = vec![
        ("s".to_string(), dispute.status.to_string()),
        ("y".to_string(), "mostrop2p".to_string()),
        ("z".to_string(), "dispute".to_string()),
    ];
    // nip33 kind with dispute id as identifier
    let event = new_event(my_keys, "", dispute.id.to_string(), tags)?;
    info!("Dispute event to be published: {event:#?}");
    NOSTR_CLIENT.get().unwrap().send_event(event).await?;

    Ok(())
}
