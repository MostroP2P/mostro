use std::borrow::Cow;
use std::str::FromStr;

use crate::db::{find_dispute_by_order_id, is_assigned_solver};
use crate::lightning::LndConnector;
use crate::nip33::new_event;
use crate::util::{enqueue_order_msg, get_nostr_client, get_order, send_dm, update_order_event};

use mostro_core::dispute::Status as DisputeStatus;
use mostro_core::error::{
    CantDoReason,
    MostroError::{self, *},
    ServiceError,
};
use mostro_core::message::{Action, Message};
use mostro_core::order::Status;
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
) -> Result<(), MostroError> {
    // Get request id
    let request_id = msg.get_inner_message_kind().request_id;
    // Get order
    let order = get_order(&msg, pool).await?;
    // Check if the solver is assigned to the order
    match is_assigned_solver(pool, &event.sender.to_string(), order.id).await {
        Ok(false) => {
            return Err(MostroCantDo(CantDoReason::IsNotYourDispute));
        }
        Err(e) => {
            return Err(MostroInternalErr(ServiceError::DbAccessError(
                e.to_string(),
            )));
        }
        _ => {}
    }

    // Was order cooperatively cancelled?
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

    // Was order in dispute?
    if order.check_status(Status::Dispute).is_err() {
        return Err(MostroCantDo(CantDoReason::NotAllowedByStatus));
    }

    if order.hash.is_some() {
        // We return funds to seller
        if let Some(hash) = order.hash.as_ref() {
            ln_client.cancel_hold_invoice(hash).await?;
            info!("Order Id {}: Funds returned to seller", &order.id);
        }
    }

    // we check if there is a dispute
    let dispute = find_dispute_by_order_id(pool, order.id).await;

    if let Ok(mut d) = dispute {
        let dispute_id = d.id;
        // we update the dispute
        d.status = DisputeStatus::SellerRefunded.to_string();
        d.update(pool)
            .await
            .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
        // We create a tag to show status of the dispute
        let tags: Tags = Tags::from_list(vec![
            Tag::custom(
                TagKind::Custom(Cow::Borrowed("s")),
                vec![DisputeStatus::SellerRefunded.to_string()],
            ),
            Tag::custom(
                TagKind::Custom(Cow::Borrowed("y")),
                vec!["mostro".to_string()],
            ),
            Tag::custom(
                TagKind::Custom(Cow::Borrowed("z")),
                vec!["dispute".to_string()],
            ),
        ]);
        // nip33 kind with dispute id as identifier
        let event = new_event(my_keys, "", dispute_id.to_string(), tags)
            .map_err(|e| MostroInternalErr(ServiceError::NostrError(e.to_string())))?;

        match get_nostr_client() {
            Ok(client) => {
                if let Err(e) = client.send_event(&event).await {
                    error!("Failed to send dispute status event: {}", e);
                }
            }
            Err(e) => error!("Failed to get Nostr client: {}", e),
        }
    }

    // We publish a new replaceable kind nostr event with the status updated
    // and update on local database the status and new event id
    let order_updated = update_order_event(my_keys, Status::CanceledByAdmin, &order)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
    order_updated
        .update(pool)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
    // We create a Message for cancel
    let message = Message::new_order(
        Some(order.id),
        request_id,
        msg.get_inner_message_kind().trade_index,
        Action::AdminCanceled,
        None,
    );

    let message = message
        .as_json()
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;
    // Message to admin
    let sender_keys = crate::util::get_keys()?;
    send_dm(event.rumor.pubkey, sender_keys, message.clone(), None)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

    let (seller_pubkey, buyer_pubkey) = match (&order.seller_pubkey, &order.buyer_pubkey) {
        (Some(seller), Some(buyer)) => (
            PublicKey::from_str(seller.as_str())
                .map_err(|_| MostroInternalErr(ServiceError::InvalidPubkey))?,
            PublicKey::from_str(buyer.as_str())
                .map_err(|_| MostroInternalErr(ServiceError::InvalidPubkey))?,
        ),
        (None, _) => return Err(MostroInternalErr(ServiceError::InvalidPubkey)),
        (_, None) => return Err(MostroInternalErr(ServiceError::InvalidPubkey)),
    };
    let sender_keys = crate::util::get_keys()?;
    send_dm(seller_pubkey, sender_keys.clone(), message.clone(), None)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::NostrError(e.to_string())))?;
    send_dm(buyer_pubkey, sender_keys, message, None)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::NostrError(e.to_string())))?;

    Ok(())
}
