//! This module handles dispute-related functionality for the P2P trading system.
//! It provides mechanisms for users to initiate disputes, notify counterparties,
//! and publish dispute events to the network.

use std::borrow::Cow;
use std::str::FromStr;

use crate::db::find_dispute_by_order_id;
use crate::nip33::new_event;
use crate::util::{get_nostr_client, send_cant_do_msg, send_new_order_msg};

use anyhow::{Error, Result};
use mostro_core::dispute::Dispute;
use mostro_core::message::{Action, Content, Message};
use mostro_core::order::{Order, Status};
use nostr::nips::nip59::UnwrappedGift;
use nostr_sdk::prelude::*;
use rand::Rng;
use sqlx::{Pool, Sqlite};
use sqlx_crud::traits::Crud;
use uuid::Uuid;

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
        .map_err(|_| Error::msg("Failed to create dispute event"))?;

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
                Err(Error::msg("Failed to send dispute event"))
            }
        },
        Err(e) => {
            tracing::error!("Failed to get Nostr client: {}", e);
            Err(Error::msg("Failed to get Nostr client"))
        }
    }
}

/// Gets information about the counterparty in a dispute.
///
/// Returns a tuple containing:
/// - The counterparty's public key as a String
/// - A boolean indicating if the dispute was initiated by the buyer (true) or seller (false)
fn get_counterpart_info(sender: &str, buyer: &str, seller: &str) -> Result<(String, bool)> {
    match sender {
        s if s == buyer => Ok((seller.to_string(), true)), // buyer is initiator
        s if s == seller => Ok((buyer.to_string(), false)), // seller is initiator
        _ => {
            tracing::error!("Message sender {sender} is neither buyer nor seller");
            Err(Error::msg("Invalid message sender"))
        }
    }
}

/// Validates and retrieves an order from the database.
///
/// Checks that:
/// - The order exists
/// - The order status allows disputes (Active or FiatSent)
async fn get_valid_order(
    pool: &Pool<Sqlite>,
    order_id: Uuid,
    event: &UnwrappedGift,
    request_id: Option<u64>,
) -> Result<Order> {
    // Try to fetch the order from the database
    let order = match Order::by_id(pool, order_id).await? {
        Some(order) => order,
        None => {
            tracing::error!("Order Id {order_id} not found!");
            return Err(Error::msg("Order not found"));
        }
    };

    // Parse and validate the order status
    match Status::from_str(&order.status) {
        Ok(status) => {
            // Only allow disputes for Active or FiatSent orders
            if !matches!(status, Status::Active | Status::FiatSent) {
                // Notify the sender that the action is not allowed for this status
                send_new_order_msg(
                    request_id,
                    Some(order.id),
                    Action::NotAllowedByStatus,
                    None,
                    &event.sender,
                )
                .await;
                return Err(Error::msg(format!(
                    "Order {} with status {} does not allow disputes. Must be Active or FiatSent",
                    order.id, order.status
                )));
            }
        }
        Err(_) => {
            return Err(Error::msg("Invalid order status"));
        }
    };

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
) -> Result<()> {
    // Get request id
    let request_id = msg.get_inner_message_kind().request_id;

    let order_id = if let Some(order_id) = msg.get_inner_message_kind().id {
        order_id
    } else {
        return Err(Error::msg("No order id"));
    };

    // Check dispute for this order id is yet present.
    if find_dispute_by_order_id(pool, order_id).await.is_ok() {
        return Err(Error::msg(format!(
            "Dispute already exists for order {}",
            order_id
        )));
    }

    // Get and validate order
    let mut order = get_valid_order(pool, order_id, event, request_id).await?;

    let (seller, buyer) = match (&order.seller_pubkey, &order.buyer_pubkey) {
        (Some(seller), Some(buyer)) => (seller.to_owned(), buyer.to_owned()),
        (None, _) => return Err(Error::msg("Missing seller pubkey")),
        (_, None) => return Err(Error::msg("Missing buyer pubkey")),
    };

    let message_sender = event.sender.to_string();
    let (counterpart, is_buyer_dispute) =
        match get_counterpart_info(&message_sender, &buyer, &seller) {
            Ok((counterpart, is_buyer_dispute)) => (counterpart, is_buyer_dispute),
            Err(_) => {
                send_cant_do_msg(request_id, Some(order.id), None, &event.sender).await;
                return Ok(());
            }
        };

    // Get the opposite dispute status
    let is_seller_dispute = !is_buyer_dispute;

    // Update dispute flags based on who initiated
    let mut update_seller_dispute = false;
    let mut update_buyer_dispute = false;
    if is_seller_dispute && !order.seller_dispute {
        update_seller_dispute = true;
        order.seller_dispute = update_seller_dispute;
    } else if is_buyer_dispute && !order.buyer_dispute {
        update_buyer_dispute = true;
        order.buyer_dispute = update_buyer_dispute;
    };
    order.status = Status::Dispute.to_string();

    // Update the database with dispute information
    // Save the dispute to DB
    if !update_buyer_dispute && !update_seller_dispute {
        return Ok(());
    } else {
        // Need to update dispute status
        order.update(pool).await?;
    }

    // Create new dispute record and generate security tokens
    let mut dispute = Dispute::new(order_id);
    let mut rng = rand::thread_rng();
    dispute.buyer_token = Some(rng.gen_range(100..=999));
    dispute.seller_token = Some(rng.gen_range(100..=999));

    let (initiator_token, counterpart_token) = match is_seller_dispute {
        true => (dispute.seller_token, dispute.buyer_token),
        false => (dispute.buyer_token, dispute.seller_token),
    };

    // Save dispute to database
    let dispute = dispute.create(pool).await?;

    // Send notification to dispute initiator
    let initiator_pubkey = match PublicKey::from_str(&message_sender) {
        Ok(pk) => pk,
        Err(e) => {
            tracing::error!("Error parsing initiator pubkey: {:#?}", e);
            return Err(Error::msg("Failed to parse initiator public key"));
        }
    };

    send_new_order_msg(
        msg.get_inner_message_kind().request_id,
        Some(order_id),
        Action::DisputeInitiatedByYou,
        Some(Content::Dispute(dispute.clone().id, initiator_token)),
        &initiator_pubkey,
    )
    .await;

    // Send notification to counterparty
    let counterpart_pubkey = match PublicKey::from_str(&counterpart) {
        Ok(pk) => pk,
        Err(e) => {
            tracing::error!("Error parsing counterpart pubkey: {:#?}", e);
            return Err(Error::msg("Failed to parse counterpart public key"));
        }
    };
    send_new_order_msg(
        msg.get_inner_message_kind().request_id,
        Some(order_id),
        Action::DisputeInitiatedByPeer,
        Some(Content::Dispute(dispute.clone().id, counterpart_token)),
        &counterpart_pubkey,
    )
    .await;

    // Publish dispute event to network
    publish_dispute_event(&dispute, my_keys).await?;
    Ok(())
}
