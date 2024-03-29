use crate::db::find_dispute_by_order_id;
use crate::lightning::LndConnector;
use crate::nip33::new_event;
use crate::util::{send_dm, settle_seller_hold_invoice, update_order_event};
use crate::NOSTR_CLIENT;

use anyhow::Result;
use mostro_core::dispute::Status as DisputeStatus;
use mostro_core::message::{Action, Message};
use mostro_core::order::{Order, Status};
use nostr_sdk::prelude::*;
use sqlx::{Pool, Sqlite};
use sqlx_crud::Crud;
use std::str::FromStr;
use tracing::error;

use super::release::do_payment;

pub async fn admin_settle_action(
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

    settle_seller_hold_invoice(event, my_keys, ln_client, Action::AdminSettle, true, &order)
        .await?;

    let order_updated = update_order_event(my_keys, Status::SettledHoldInvoice, &order).await?;

    // we check if there is a dispute
    let dispute = find_dispute_by_order_id(pool, order_id).await;

    if let Ok(mut d) = dispute {
        let dispute_id = d.id;
        // we update the dispute
        d.status = DisputeStatus::Settled;
        d.update(pool).await?;
        // We create a tag to show status of the dispute
        let tags = vec![
            ("s".to_string(), DisputeStatus::Settled.to_string()),
            ("y".to_string(), "mostrop2p".to_string()),
            ("z".to_string(), "dispute".to_string()),
        ];
        // nip33 kind with dispute id as identifier
        let event = new_event(my_keys, "", dispute_id.to_string(), tags)?;

        NOSTR_CLIENT.get().unwrap().send_event(event).await?;
    }
    // We create a Message
    let message = Message::new_dispute(Some(order_updated.id), None, Action::AdminSettle, None);
    let message = message.as_json()?;
    // Message to admin
    send_dm(&event.pubkey, message.clone()).await?;
    let seller_pubkey = order_updated.seller_pubkey.as_ref().unwrap();
    let seller_pubkey = PublicKey::from_str(seller_pubkey).unwrap();
    send_dm(&seller_pubkey, message.clone()).await?;
    let buyer_pubkey = order_updated.buyer_pubkey.as_ref().unwrap();
    let buyer_pubkey = PublicKey::from_str(buyer_pubkey).unwrap();
    send_dm(&buyer_pubkey, message.clone()).await?;

    let _ = do_payment(order_updated).await;

    Ok(())
}
