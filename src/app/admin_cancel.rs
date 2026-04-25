use std::borrow::Cow;
use std::str::FromStr;

use crate::app::context::AppContext;
use crate::db::{
    find_dispute_by_order_id, is_assigned_solver, is_dispute_taken_by_admin,
    solver_has_write_permission,
};
use crate::lightning::LndConnector;
use crate::nip33::{create_platform_tag_values, new_dispute_event};
use crate::util::{enqueue_order_msg, get_order, send_dm, update_order_event};
use mostro_core::prelude::*;
use nostr_sdk::prelude::*;
use sqlx_crud::Crud;
use tracing::{error, info};

/// Admin-initiated order cancellation.
///
/// Allows authorized dispute solvers or admins to cancel an order and refund
/// any held Lightning invoice back to the seller.
///
/// # Parameters
///
/// * `ctx` - Application context containing DB pool, settings, and message queue
/// * `msg` - Incoming message with the order ID and request metadata
/// * `event` - Unwrapped NIP-59 message exposing `sender` (trade key, rumor
///   author) and `identity` (long-lived identity key, seal signer); admin
///   gating is performed against `event.identity`
/// * `my_keys` - Mostro daemon's signing keys
/// * `ln_client` - Lightning network client for hold invoice cancellation
///
/// # Side Effects
///
/// - Cancels Lightning hold invoice (if present)
/// - Updates order status to `CanceledByAdmin` in database
/// - Publishes updated order event to Nostr
/// - Sends direct messages to both buyer and seller
///
/// # Errors
///
/// Returns `MostroError` if:
/// - Solver is not assigned to the dispute
/// - Order/dispute not found
/// - Lightning invoice cancellation fails
/// - Database update fails
/// - Nostr publish fails
pub async fn admin_cancel_action(
    ctx: &AppContext,
    msg: Message,
    event: &UnwrappedMessage,
    my_keys: &Keys,
    ln_client: &mut LndConnector,
) -> Result<(), MostroError> {
    let pool = ctx.pool();
    // Get request id
    let request_id = msg.get_inner_message_kind().request_id;
    // Get order
    let order = get_order(&msg, pool).await?;
    // Check if the solver is assigned to the order
    match is_assigned_solver(pool, &event.identity.to_string(), order.id).await {
        Ok(false) => {
            // Check if admin has taken over the dispute
            if is_dispute_taken_by_admin(pool, order.id, &my_keys.public_key().to_string()).await? {
                return Err(MostroCantDo(CantDoReason::DisputeTakenByAdmin));
            } else {
                return Err(MostroCantDo(CantDoReason::IsNotYourDispute));
            }
        }
        Err(e) => {
            return Err(MostroInternalErr(ServiceError::DbAccessError(
                e.to_string(),
            )));
        }
        _ => {}
    }

    if !solver_has_write_permission(pool, &event.identity.to_string(), order.id).await? {
        return Err(MostroCantDo(CantDoReason::NotAuthorized));
    }

    // Was order cooperatively cancelled?
    if order.check_status(Status::CooperativelyCanceled).is_ok() {
        enqueue_order_msg(
            request_id,
            Some(order.id),
            Action::CooperativeCancelAccepted,
            None,
            event.identity,
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

    // Get the creator of the dispute
    let dispute_initiator = match (order.seller_dispute, order.buyer_dispute) {
        (true, false) => "seller",
        (false, true) => "buyer",
        (_, _) => return Err(MostroInternalErr(ServiceError::DisputeEventError)),
    };

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
            // Who is the dispute creator
            Tag::custom(
                TagKind::Custom(std::borrow::Cow::Borrowed("initiator")),
                vec![dispute_initiator],
            ),
            Tag::custom(
                TagKind::Custom(Cow::Borrowed("y")),
                create_platform_tag_values(ctx.settings().mostro.name.as_deref()),
            ),
            Tag::custom(
                TagKind::Custom(Cow::Borrowed("z")),
                vec!["dispute".to_string()],
            ),
        ]);
        // nip33 kind with dispute id as identifier (kind 38386 for disputes)
        let event = new_dispute_event(my_keys, "", dispute_id.to_string(), tags)
            .map_err(|e| MostroInternalErr(ServiceError::NostrError(e.to_string())))?;

        // Publish dispute event with update
        info!("Dispute event to be published: {event:#?}");

        let client = ctx.nostr_client();
        if let Err(e) = client.send_event(&event).await {
            error!("Failed to send dispute status event: {}", e);
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
    send_dm(event.sender, my_keys, &message, None)
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
    send_dm(seller_pubkey, my_keys, &message, None)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::NostrError(e.to_string())))?;
    send_dm(buyer_pubkey, my_keys, &message, None)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::NostrError(e.to_string())))?;

    Ok(())
}
