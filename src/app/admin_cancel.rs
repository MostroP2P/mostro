use std::borrow::Cow;
use std::str::FromStr;

use crate::db::{find_dispute_by_order_id, is_assigned_solver};
use crate::lightning::LndConnector;
use crate::nip33::new_event;
use crate::util::{get_nostr_client, send_dm, send_new_order_msg, update_order_event};

use anyhow::{Error, Result};
use mostro_core::dispute::Status as DisputeStatus;
use mostro_core::message::{Action, Message, MessageKind};
use mostro_core::order::{Order, Status};
use nostr::nips::nip59::UnwrappedGift;
use nostr_sdk::prelude::*;
use sqlx::{Pool, Sqlite};
use sqlx_crud::Crud;
use tracing::{error, info};

pub async fn admin_cancel_action(
    msg: Message,
    event: &UnwrappedGift,
    my_keys: &Keys,
    pool: &Pool<Sqlite>,
    ln_client: &mut LndConnector,
) -> Result<()> {
    // Get request id
    let request_id = msg.get_inner_message_kind().request_id;

    let order_id = if let Some(order_id) = msg.get_inner_message_kind().id {
        order_id
    } else {
        return Err(Error::msg("No order id"));
    };
    let inner_message = msg.get_inner_message_kind();

    match is_assigned_solver(pool, &event.sender.to_string(), order_id).await {
        Ok(false) => {
            send_new_order_msg(
                inner_message.request_id,
                Some(order_id),
                Action::IsNotYourDispute,
                None,
                &event.sender,
                inner_message.trade_index,
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

    // Was order cooperatively cancelled?
    if order.status == Status::CooperativelyCanceled.to_string() {
        let message = MessageKind::new(
            Some(order_id),
            request_id,
            inner_message.trade_index,
            Action::CooperativeCancelAccepted,
            None,
        );
        if let Ok(message) = message.as_json() {
            let sender_keys = crate::util::get_keys().unwrap();
            let _ = send_dm(&event.sender, sender_keys, message).await;
        }
        return Ok(());
    }

    if order.status != Status::Dispute.to_string() {
        send_new_order_msg(
            inner_message.request_id,
            Some(order.id),
            Action::NotAllowedByStatus,
            None,
            &event.sender,
            inner_message.trade_index,
        )
        .await;
        return Ok(());
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
        let tags: Tags = Tags::new(vec![
            Tag::custom(
                TagKind::Custom(Cow::Borrowed("s")),
                vec![DisputeStatus::SellerRefunded.to_string()],
            ),
            Tag::custom(
                TagKind::Custom(Cow::Borrowed("y")),
                vec!["mostrop2p".to_string()],
            ),
            Tag::custom(
                TagKind::Custom(Cow::Borrowed("z")),
                vec!["dispute".to_string()],
            ),
        ]);
        // nip33 kind with dispute id as identifier
        let event = new_event(my_keys, "", dispute_id.to_string(), tags)?;

        match get_nostr_client() {
            Ok(client) => {
                if let Err(e) = client.send_event(event).await {
                    error!("Failed to send dispute status event: {}", e);
                }
            }
            Err(e) => error!("Failed to get Nostr client: {}", e),
        }
    }

    // We publish a new replaceable kind nostr event with the status updated
    // and update on local database the status and new event id
    let order_updated = update_order_event(my_keys, Status::CanceledByAdmin, &order).await?;
    order_updated.update(pool).await?;
    // We create a Message for cancel
    let message = Message::new_order(
        Some(order.id),
        request_id,
        inner_message.trade_index,
        Action::AdminCanceled,
        None,
    );
    let message = message.as_json()?;
    // Message to admin
    let sender_keys = crate::util::get_keys().unwrap();
    send_dm(&event.sender, sender_keys, message.clone()).await?;

    let (seller_pubkey, buyer_pubkey) = match (&order.seller_pubkey, &order.buyer_pubkey) {
        (Some(seller), Some(buyer)) => (
            PublicKey::from_str(seller.as_str())?,
            PublicKey::from_str(buyer.as_str())?,
        ),
        (None, _) => return Err(Error::msg("Missing seller pubkey")),
        (_, None) => return Err(Error::msg("Missing buyer pubkey")),
    };
    let sender_keys = crate::util::get_keys().unwrap();
    send_dm(&seller_pubkey, sender_keys.clone(), message.clone()).await?;
    send_dm(&buyer_pubkey, sender_keys, message).await?;

    Ok(())
}
