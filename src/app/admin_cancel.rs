use std::borrow::Cow;
use std::str::FromStr;

use crate::db::{find_dispute_by_order_id, is_assigned_solver};
use crate::lightning::LndConnector;
use crate::nip33::new_event;
use crate::util::{
    enqueue_order_msg, get_nostr_client, get_order, send_cant_do_msg, send_dm, update_order_event,
};

use mostro_core::dispute::Status as DisputeStatus;
use mostro_core::error::{
    CantDoReason,
    MostroError::{self, *},
    ServiceError,
};
use mostro_core::message::{Action, Message, MessageKind};
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
    match is_assigned_solver(pool, &event.rumor.pubkey.to_string(), order.id).await {
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
    if let Err(cause) = order.check_status(Status::CooperativelyCanceled) {
        return Err(MostroCantDo(cause));
    } else {
        enqueue_order_msg(
            request_id,
            Some(order.id),
            Action::CooperativeCancelAccepted,
            None,
            event.rumor.pubkey,
            msg.get_inner_message_kind().trade_index,
        )
        .await;
    }

    // Was order in dispute?
    if let Ok(_) = order.check_status(Status::Dispute) {
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
    send_dm(&event.rumor.pubkey, sender_keys, message.clone(), None).await?;

    let (seller_pubkey, buyer_pubkey) = match (&order.seller_pubkey, &order.buyer_pubkey) {
        (Some(seller), Some(buyer)) => (
            PublicKey::from_str(seller.as_str())?,
            PublicKey::from_str(buyer.as_str())?,
        ),
        (None, _) => return Err(Error::msg("Missing seller pubkey")),
        (_, None) => return Err(Error::msg("Missing buyer pubkey")),
    };
    let sender_keys = crate::util::get_keys().unwrap();
    send_dm(&seller_pubkey, sender_keys.clone(), message.clone(), None).await?;
    send_dm(&buyer_pubkey, sender_keys, message, None).await?;

    Ok(())
}
