use std::str::FromStr;

use crate::db::find_dispute_by_order_id;
use crate::lightning::LndConnector;
use crate::nip33::new_event;
use crate::util::{send_cant_do_msg, send_dm, update_order_event};
use crate::NOSTR_CLIENT;

use anyhow::Result;
use mostro_core::dispute::Status as DisputeStatus;
use mostro_core::message::{Action, Message};
use mostro_core::order::{Order, Status};
use nostr_sdk::prelude::*;
use sqlx::{Pool, Sqlite};
use sqlx_crud::Crud;
use tracing::{error, info};

pub async fn admin_cancel_action(
    msg: Message,
    event: &Event,
    my_keys: &Keys,
    pool: &Pool<Sqlite>,
    ln_client: &mut LndConnector,
) -> Result<()> {
    let order_id = msg.get_inner_message_kind().id.unwrap();
    let order = match Order::by_id(pool, order_id).await? {
        Some(order) => order,
        None => {
            error!("Order Id {order_id} not found!");
            return Ok(());
        }
    };

    // Check if the pubkey is Mostro
    if event.pubkey.to_string() != my_keys.public_key().to_string() {
        // We create a Message
        send_cant_do_msg(Some(order_id), None, &event.pubkey).await;
        return Ok(());
    }

    if order.hash.is_some() {
        // We return funds to seller
        let hash = order.hash.as_ref().unwrap();
        ln_client.cancel_hold_invoice(hash).await?;
        info!("Order Id {}: Funds returned to seller", &order.id);
    }

    // we check if there is a dispute
    let dispute = find_dispute_by_order_id(pool, order_id).await;

    if let Ok(mut d) = dispute {
        let dispute_id = d.id;
        // we update the dispute
        d.status = DisputeStatus::SellerRefunded;
        d.update(pool).await?;
        // We create a tag to show status of the dispute
        let tags = vec![
            ("s".to_string(), DisputeStatus::SellerRefunded.to_string()),
            ("y".to_string(), "mostrop2p".to_string()),
            ("z".to_string(), "dispute".to_string()),
        ];
        // nip33 kind with dispute id as identifier
        let event = new_event(my_keys, "", dispute_id.to_string(), tags)?;

        NOSTR_CLIENT.get().unwrap().send_event(event).await?;
    }

    // We publish a new replaceable kind nostr event with the status updated
    // and update on local database the status and new event id
    let order_updated = update_order_event(my_keys, Status::CanceledByAdmin, &order).await?;
    order_updated.update(pool).await?;
    // We create a Message
    let message = Message::new_dispute(Some(order.id), None, Action::AdminCancel, None);
    let message = message.as_json()?;
    // Message to admin
    send_dm(&event.pubkey, message.clone()).await?;
    let seller_pubkey = match XOnlyPublicKey::from_str(order.seller_pubkey.as_ref().unwrap()) {
        Ok(pk) => pk,
        Err(e) => {
            error!("Error parsing seller pubkey: {:#?}", e);
            return Ok(());
        }
    };
    send_dm(&seller_pubkey, message.clone()).await?;
    let buyer_pubkey = match XOnlyPublicKey::from_str(order.buyer_pubkey.as_ref().unwrap()) {
        Ok(pk) => pk,
        Err(e) => {
            error!("Error parsing buyer pubkey: {:#?}", e);
            return Ok(());
        }
    };
    send_dm(&buyer_pubkey, message.clone()).await?;

    Ok(())
}
