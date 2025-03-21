use crate::db::{find_dispute_by_order_id, is_assigned_solver};
use crate::lightning::LndConnector;
use crate::nip33::new_event;
use crate::util::{
    enqueue_order_msg, get_nostr_client, get_order, settle_seller_hold_invoice, update_order_event,
};

use mostro_core::dispute::Status as DisputeStatus;
use mostro_core::error::MostroError::{self, *};
use mostro_core::error::ServiceError;
use mostro_core::message::{Action, Message};
use mostro_core::order::Status;
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
) -> Result<(), MostroError> {
    // Get request id
    let request_id = msg.get_inner_message_kind().request_id;
    // Get order
    let order = get_order(&msg, pool).await?;

    match is_assigned_solver(pool, &event.sender.to_string(), order.id).await {
        Ok(false) => {
            return Err(MostroCantDo(
                mostro_core::error::CantDoReason::IsNotYourDispute,
            ));
        }
        Err(e) => {
            return Err(MostroInternalErr(ServiceError::DbAccessError(
                e.to_string(),
            )));
        }
        _ => {}
    }

    // Was orde cooperatively cancelled?
    if order.check_status(Status::CooperativelyCanceled).is_ok() {
        enqueue_order_msg(
            request_id,
            Some(order.id),
            Action::CooperativeCancelAccepted,
            None,
            event.sender,
            msg.get_inner_message_kind().trade_index,
        )
        .await;

        return Ok(());
    }

    if let Err(cause) = order.check_status(Status::Dispute) {
        return Err(MostroCantDo(cause));
    }
    // Settle seller hold invoice
    settle_seller_hold_invoice(event, ln_client, Action::AdminSettled, true, &order)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::LnNodeError(e.to_string())))?;
    // Update order event
    let order_updated = update_order_event(my_keys, Status::SettledHoldInvoice, &order)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

    // we check if there is a dispute
    let dispute = find_dispute_by_order_id(pool, order.id).await;

    if let Ok(mut d) = dispute {
        let dispute_id = d.id;
        // we update the dispute
        d.status = DisputeStatus::Settled.to_string();
        d.update(pool)
            .await
            .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
        // We create a tag to show status of the dispute
        let tags: Tags = Tags::new(vec![
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
        ]);

        // nip33 kind with dispute id as identifier
        let event = new_event(my_keys, "", dispute_id.to_string(), tags)
            .map_err(|e| MostroInternalErr(ServiceError::NostrError(e.to_string())))?;

        match get_nostr_client() {
            Ok(client) => {
                if let Err(e) = client.send_event(event).await {
                    error!("Failed to send dispute settlement event: {}", e);
                }
            }
            Err(e) => {
                error!("Failed to get Nostr client for dispute settlement: {}", e);
            }
        }
    }

    // Send message to event creator
    enqueue_order_msg(
        request_id,
        Some(order_updated.id),
        Action::AdminSettled,
        None,
        event.rumor.pubkey,
        msg.get_inner_message_kind().trade_index,
    )
    .await;

    // Send message to seller and buyer
    if let Some(ref seller_pubkey) = order_updated.seller_pubkey {
        enqueue_order_msg(
            None,
            Some(order_updated.id),
            Action::AdminSettled,
            None,
            PublicKey::from_str(seller_pubkey)
                .map_err(|_| MostroInternalErr(ServiceError::InvalidPubkey))?,
            msg.get_inner_message_kind().trade_index,
        )
        .await;
    }
    // Send message to buyer
    if let Some(ref buyer_pubkey) = order_updated.buyer_pubkey {
        enqueue_order_msg(
            None,
            Some(order_updated.id),
            Action::AdminSettled,
            None,
            PublicKey::from_str(buyer_pubkey)
                .map_err(|_| MostroInternalErr(ServiceError::InvalidPubkey))?,
            msg.get_inner_message_kind().trade_index,
        )
        .await;
    }
    let _ = do_payment(order_updated, request_id).await;

    Ok(())
}
