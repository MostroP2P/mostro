use std::str::FromStr;

use crate::db::{find_dispute_by_order_id, find_solver_pubkey, is_assigned_solver};
use crate::lightning::LndConnector;
use crate::nip33::new_event;
use crate::util::{send_cant_do_msg, send_dm, update_order_event};
use crate::NOSTR_CLIENT;

use anyhow::{Error, Result};
use mostro_core::dispute::Status as DisputeStatus;
use mostro_core::message::{Action, Message, MessageKind};
use mostro_core::order::{Order, Status};
use nostr_sdk::prelude::*;
use sqlx::{Pool, Sqlite};
use sqlx_crud::Crud;
use tracing::{error, info};

pub async fn pubkey_event_can_solve(pool: &Pool<Sqlite>, ev_pubkey: &PublicKey) -> bool {
    if let Ok(my_keys) = crate::util::get_keys() {
        // Is mostro admin taking dispute?
        if ev_pubkey.to_string() == my_keys.public_key().to_string() {
            return true;
        }
    }

    // Is a solver taking a dispute
    if let Ok(solver) = find_solver_pubkey(pool, ev_pubkey.to_string()).await {
        if solver.is_solver != 0_i64 {
            return true;
        }
    }

    false
}

pub async fn admin_cancel_action(
    msg: Message,
    event: &Event,
    my_keys: &Keys,
    pool: &Pool<Sqlite>,
    ln_client: &mut LndConnector,
) -> Result<()> {
    let order_id = if let Some(order_id) = msg.get_inner_message_kind().id {
        order_id
    } else {
        return Err(Error::msg("No order id"));
    };

    let order = match Order::by_id(pool, order_id).await? {
        Some(order) => order,
        None => {
            error!("Order Id {order_id} not found!");
            return Ok(());
        }
    };

    // Check if the pubkey is a solver or admin
    if !pubkey_event_can_solve(pool, &event.pubkey).await {
        send_cant_do_msg(Some(order.id), None, &event.pubkey).await;
        return Ok(());
    }

    // Was order cooperatively cancelled?
    if order.status == Status::CooperativelyCanceled.to_string() {
        let message = MessageKind::new(
            Some(order_id),
            Some(event.pubkey.to_string()),
            Action::CooperativeCancelAccepted,
            None,
        );
        if let Ok(message) = message.as_json() {
            let _ = send_dm(&event.pubkey, message).await;
        }
        return Ok(());
    }

    if order.status != Status::Dispute.to_string() {
        let error = format!(
            "Can't cancel an order with status different than {}!",
            Status::Dispute
        );
        send_cant_do_msg(Some(order.id), Some(error), &event.pubkey).await;

        return Ok(());
    }

    match is_assigned_solver(pool, &event.pubkey.to_string(), order_id).await {
        Ok(false) => {
            send_cant_do_msg(
                None,
                Some("Dispute not taken by you".to_string()),
                &event.pubkey,
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

    if order.hash.is_some() {
        // We return funds to seller
        if let Some(hash) = order.hash.as_ref() {
            ln_client.cancel_hold_invoice(hash).await?;
            info!("Order Id {}: Funds returned to seller", &order.id);
        }
    }

    // we check if there is a dispute
    let dispute = find_dispute_by_order_id(pool, order_id).await;

    if let Ok(mut d) = dispute {
        let dispute_id = d.id;
        // we update the dispute
        d.status = DisputeStatus::SellerRefunded.to_string();
        d.update(pool).await?;
        // We create a tag to show status of the dispute
        let tags: Vec<Tag> = vec![
            Tag::custom(
                TagKind::Custom(std::borrow::Cow::Borrowed("s")),
                vec![DisputeStatus::SellerRefunded.to_string()],
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

    // We publish a new replaceable kind nostr event with the status updated
    // and update on local database the status and new event id
    let order_updated = update_order_event(my_keys, Status::CanceledByAdmin, &order).await?;
    order_updated.update(pool).await?;
    // We create a Message for cancel
    let message = Message::new_dispute(Some(order.id), None, Action::AdminCanceled, None);
    let message = message.as_json()?;
    // Message to admin
    send_dm(&event.pubkey, message.clone()).await?;

    let (seller_pubkey, buyer_pubkey) = match (&order.seller_pubkey, &order.buyer_pubkey) {
        (Some(seller), Some(buyer)) => (
            PublicKey::from_str(seller.as_str())?,
            PublicKey::from_str(buyer.as_str())?,
        ),
        (None, _) => return Err(Error::msg("Missing seller pubkey")),
        (_, None) => return Err(Error::msg("Missing buyer pubkey")),
    };

    send_dm(&seller_pubkey, message.clone()).await?;
    send_dm(&buyer_pubkey, message).await?;

    Ok(())
}
