use std::str::FromStr;

use crate::nip33::new_event;
use crate::util::send_dm;

use anyhow::Result;
use mostro_core::dispute::Dispute;
use mostro_core::message::{Action, Message};
use mostro_core::order::Order;
use nostr_sdk::prelude::*;
use sqlx::{Pool, Sqlite};
use sqlx_crud::traits::Crud;
use tracing::{error, info};

pub async fn dispute_action(
    msg: Message,
    event: &Event,
    my_keys: &Keys,
    client: &Client,
    pool: &Pool<Sqlite>,
) -> Result<()> {
    let order_id = msg.get_inner_message_kind().id.unwrap();
    let mut order = match Order::by_id(pool, order_id).await? {
        Some(order) => order,
        None => {
            error!("Order Id {order_id} not found!");
            return Ok(());
        }
    };

    let buyer = order.buyer_pubkey.clone().unwrap();
    let seller = order.seller_pubkey.clone().unwrap();
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
        let message = Message::cant_do(Some(order.id), None, None);
        let message = message.as_json()?;
        send_dm(client, my_keys, &event.pubkey, message).await?;

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
    let message = Message::new_order(Some(order_id), None, Action::DisputeInitiatedByYou, None);
    let message = message.as_json()?;
    let initiator_pubkey = match XOnlyPublicKey::from_str(&message_sender) {
        Ok(pk) => pk,
        Err(e) => {
            error!("Error parsing initiator pubkey: {:#?}", e);
            return Ok(());
        }
    };
    send_dm(client, my_keys, &initiator_pubkey, message).await?;

    // We create a Message for the counterpart
    let message = Message::new_order(Some(order_id), None, Action::DisputeInitiatedByPeer, None);
    let message = message.as_json()?;
    let counterpart_pubkey = match XOnlyPublicKey::from_str(&counterpart) {
        Ok(pk) => pk,
        Err(e) => {
            error!("Error parsing counterpart pubkey: {:#?}", e);
            return Ok(());
        }
    };
    send_dm(client, my_keys, &counterpart_pubkey, message).await?;
    // We create a tag to show status of the dispute
    let tags = vec![
        ("s".to_string(), dispute.status.to_string()),
        ("y".to_string(), "mostrop2p".to_string()),
        ("z".to_string(), "dispute".to_string()),
    ];
    // nip33 kind with dispute id as identifier
    let event = new_event(my_keys, "", dispute.id.to_string(), tags)?;
    info!("Dispute event to be published: {event:#?}");
    client.send_event(event).await?;

    Ok(())
}
