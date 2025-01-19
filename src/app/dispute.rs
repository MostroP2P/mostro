//! This module handles dispute-related functionality for the P2P trading system.
//! It provides mechanisms for users to initiate disputes, notify counterparties,
//! and publish dispute events to the network.

use std::borrow::Cow;
use std::str::FromStr;

use crate::db::find_dispute_by_order_id;
use crate::nip33::new_event;
use crate::util::{enqueue_order_msg, get_nostr_client, get_order};

use anyhow::Result;
use mostro_core::dispute::Dispute;
use mostro_core::error::{
    CantDoReason,
    MostroError::{self, *},
    ServiceError,
};
use mostro_core::message::{Action, Message, Payload};
use mostro_core::order::{Order, Status};
use nostr::nips::nip59::UnwrappedGift;
use nostr_sdk::prelude::*;
use sqlx::{Pool, Sqlite};
use sqlx_crud::traits::Crud;

/// Publishes a dispute event to the Nostr network.
///
/// Creates and publishes a NIP-33 replaceable event containing dispute details
/// including status and application metadata.
async fn publish_dispute_event(dispute: &Dispute, my_keys: &Keys) -> Result<()> {
    // Create tags for the dispute event
    let tags = Tags::new(vec![
        // Status tag - indicates the current state of the dispute
        Tag::custom(
            TagKind::Custom(Cow::Borrowed("s")),
            vec![dispute.status.to_string()],
        ),
        // Application identifier tag
        Tag::custom(
            TagKind::Custom(Cow::Borrowed("y")),
            vec!["mostrop2p".to_string()],
        ),
        // Event type tag
        Tag::custom(
            TagKind::Custom(Cow::Borrowed("z")),
            vec!["dispute".to_string()],
        ),
    ]);

    // Create a new NIP-33 replaceable event
    // Empty content string as the information is in the tags
    let event = new_event(my_keys, "", dispute.id.to_string(), tags)
        .map_err(|_| MostroInternalErr(ServiceError::DisputeEventError))?;

    tracing::info!("Publishing dispute event: {:#?}", event);

    // Get nostr client and publish the event
    match get_nostr_client() {
        Ok(client) => match client.send_event(event).await {
            Ok(_) => {
                tracing::info!(
                    "Successfully published dispute event for dispute ID: {}",
                    dispute.id
                );
                Ok(())
            }
            Err(e) => {
                tracing::error!("Failed to send dispute event: {}", e);
                Err(e.into())
            }
        },
        Err(e) => {
            tracing::error!("Failed to get Nostr client: {}", e);
            Err(e)
        }
    }
}

/// Gets information about the counterparty in a dispute.
///
/// Returns a tuple containing:
/// - The counterparty's public key as a String
/// - A boolean indicating if the dispute was initiated by the buyer (true) or seller (false)
fn get_counterpart_info(
    sender: &str,
    buyer: &str,
    seller: &str,
) -> Result<(String, bool), CantDoReason> {
    match sender {
        s if s == buyer => Ok((seller.to_string(), true)), // buyer is initiator
        s if s == seller => Ok((buyer.to_string(), false)), // seller is initiator
        _ => Err(CantDoReason::InvalidPubkey),
    }
}

/// Validates and retrieves an order from the database.
///
/// Checks that:
/// - The order exists
/// - The order status allows disputes (Active or FiatSent)
async fn get_valid_order(pool: &Pool<Sqlite>, msg: &Message) -> Result<Order, MostroError> {
    // Try to fetch the order from the database
    let order = get_order(msg, pool).await?;

    // Check if the order status is Active or FiatSent
    if order.check_status(Status::Active).is_err() && order.check_status(Status::FiatSent).is_err()
    {
        return Err(MostroCantDo(CantDoReason::NotAllowedByStatus));
    }

    Ok(order)
}

/// Main handler for dispute actions.
///
/// This function:
/// 1. Validates the order and dispute status
/// 2. Updates the order status
/// 3. Creates a new dispute record
/// 4. Generates security tokens for both parties
/// 5. Notifies both parties
/// 6. Publishes the dispute event to the network
pub async fn dispute_action(
    msg: Message,
    event: &UnwrappedGift,
    my_keys: &Keys,
    pool: &Pool<Sqlite>,
) -> Result<(), MostroError> {
    let order_id = if let Some(order_id) = msg.get_inner_message_kind().id {
        order_id
    } else {
        return Err(MostroInternalErr(ServiceError::InvalidOrderId));
    };
    // Check dispute for this order id is yet present.
    if find_dispute_by_order_id(pool, order_id).await.is_ok() {
        return Err(MostroInternalErr(ServiceError::DisputeAlreadyExists));
    }
    // Get and validate order
    let mut order = get_valid_order(pool, &msg).await?;
    // Get seller and buyer pubkeys
    let (seller, buyer) = match (&order.seller_pubkey, &order.buyer_pubkey) {
        (Some(seller), Some(buyer)) => (seller.to_owned(), buyer.to_owned()),
        (None, _) => return Err(MostroInternalErr(ServiceError::InvalidPubkey)),
        (_, None) => return Err(MostroInternalErr(ServiceError::InvalidPubkey)),
    };
    // Get message sender
    let message_sender = event.rumor.pubkey.to_string();
    // Get counterpart info
    let (counterpart, is_buyer_dispute) =
        match get_counterpart_info(&message_sender, &buyer, &seller) {
            Ok((counterpart, is_buyer_dispute)) => (counterpart, is_buyer_dispute),
            Err(cause) => return Err(MostroCantDo(cause)),
        };

    // Setup dispute
    if order.setup_dispute(is_buyer_dispute).is_ok() {
        order
            .update(pool)
            .await
            .map_err(|cause| MostroInternalErr(ServiceError::DbAccessError(cause.to_string())))?;
    }

    // Create new dispute record and generate security tokens
    let mut dispute = Dispute::new(order_id);
    // Create tokens
    let (initiator_token, counterpart_token) = dispute.create_tokens(is_buyer_dispute);

    // Save dispute to database
    let dispute = dispute
        .create(pool)
        .await
        .map_err(|cause| MostroInternalErr(ServiceError::DbAccessError(cause.to_string())))?;

    // Send notification to dispute initiator
    let initiator_pubkey = match PublicKey::from_str(&message_sender) {
        Ok(pk) => pk,
        Err(_) => {
            return Err(MostroInternalErr(ServiceError::InvalidPubkey));
        }
    };

    enqueue_order_msg(
        msg.get_inner_message_kind().request_id,
        Some(order_id),
        Action::DisputeInitiatedByYou,
        Some(Payload::Dispute(dispute.clone().id, initiator_token)),
        initiator_pubkey,
        None,
    )
    .await;

    // Send notification to counterparty
    let counterpart_pubkey = match PublicKey::from_str(&counterpart) {
        Ok(pk) => pk,
        Err(_) => {
            return Err(MostroInternalErr(ServiceError::InvalidPubkey));
        }
    };
    enqueue_order_msg(
        msg.get_inner_message_kind().request_id,
        Some(order_id),
        Action::DisputeInitiatedByPeer,
        Some(Payload::Dispute(dispute.clone().id, counterpart_token)),
        counterpart_pubkey,
        None,
    )
    .await;

    // Publish dispute event to network
    publish_dispute_event(&dispute, my_keys)
        .await
        .map_err(|_| MostroInternalErr(ServiceError::DisputeEventError))?;
    Ok(())
}
