use crate::db::{find_dispute_by_order_id, is_assigned_solver};
use crate::lightning::LndConnector;
use crate::nip33::new_event;
use crate::util::{send_dm, send_new_order_msg, settle_seller_hold_invoice, update_order_event};
use crate::NOSTR_CLIENT;

use anyhow::{Error, Result};
use mostro_core::dispute::Status as DisputeStatus;
use mostro_core::message::{Action, Message, MessageKind};
use mostro_core::order::{Order, Status};
use nostr::nips::nip59::UnwrappedGift;
use nostr_sdk::prelude::*;
use sqlx::{Pool, Sqlite};
use sqlx_crud::Crud;
use std::str::FromStr;
use tracing::error;

use super::release::do_payment;

pub async fn admin_settle_action(
    msg: Message,
    event: &UnwrappedGift,
    my_keys: &Keys,
    pool: &Pool<Sqlite>,
    ln_client: &mut LndConnector,
) -> Result<()> {
    let order_id = if let Some(order_id) = msg.get_inner_message_kind().id {
        order_id
    } else {
        return Err(Error::msg("No order id"));
    };

    match is_assigned_solver(pool, &event.sender.to_string(), order_id).await {
        Ok(false) => {
            send_new_order_msg(
                Some(order_id),
                Action::IsNotYourDispute,
                None,
                &event.sender,
            )
            .await;
            return Ok(());
        }
        Err(e) => {
            error!("Error checking if solver is assigned to order: {:?}", e);
            return Ok(());
        }
        _ => {}
    }

    let order = match Order::by_id(pool, order_id).await? {
        Some(order) => order,
        None => {
            error!("Order Id {order_id} not found!");
            return Ok(());
        }
    };

    // Was orde cooperatively cancelled?
    if order.status == Status::CooperativelyCanceled.to_string() {
        let message = MessageKind::new(Some(order_id), Action::CooperativeCancelAccepted, None);
        if let Ok(message) = message.as_json() {
            let _ = send_dm(&event.sender, message).await;
        }
        return Ok(());
    }

    if order.status != Status::Dispute.to_string() {
        send_new_order_msg(
            Some(order.id),
            Action::NotAllowedByStatus,
            None,
            &event.sender,
        )
        .await;
        return Ok(());
    }

    settle_seller_hold_invoice(event, ln_client, Action::AdminSettled, true, &order).await?;

    let order_updated = update_order_event(my_keys, Status::SettledHoldInvoice, &order).await?;

    // we check if there is a dispute
    let dispute = find_dispute_by_order_id(pool, order_id).await;

    if let Ok(mut d) = dispute {
        let dispute_id = d.id;
        // we update the dispute
        d.status = DisputeStatus::Settled.to_string();
        d.update(pool).await?;
        // We create a tag to show status of the dispute
        let tags: Vec<Tag> = vec![
            Tag::custom(
                TagKind::Custom(std::borrow::Cow::Borrowed("s")),
                vec![DisputeStatus::Settled.to_string()],
            ),
            Tag::custom(
                TagKind::Custom(std::borrow::Cow::Borrowed("y")),
                vec!["mostrop2p".to_string()],
            ),
            Tag::custom(
                TagKind::Custom(std::borrow::Cow::Borrowed("z")),
                vec!["dispute".to_string()],
            ),
        ];

        // nip33 kind with dispute id as identifier
        let event = new_event(my_keys, "", dispute_id.to_string(), tags)?;

        NOSTR_CLIENT.get().unwrap().send_event(event).await?;
    }
    // We create a Message for settle
    let message = Message::new_order(Some(order_updated.id), Action::AdminSettled, None);
    let message = message.as_json()?;
    // Message to admin
    send_dm(&event.sender, message.clone()).await?;
    if let Some(ref seller_pubkey) = order_updated.seller_pubkey {
        send_dm(&PublicKey::from_str(seller_pubkey)?, message.clone()).await?;
    }
    if let Some(ref buyer_pubkey) = order_updated.buyer_pubkey {
        send_dm(&PublicKey::from_str(buyer_pubkey)?, message.clone()).await?;
    }

    let _ = do_payment(order_updated).await;

    Ok(())
}
