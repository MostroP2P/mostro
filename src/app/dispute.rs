//! This module handles dispute-related functionality for the P2P trading system.
//! It provides mechanisms for users to initiate disputes, notify counterparties,
//! and publish dispute events to the network.

use crate::app::context::AppContext;
use crate::db::find_dispute_by_order_id;
use crate::nip33::{create_platform_tag_values, new_dispute_event};
use crate::util::{enqueue_order_msg, get_order};
use mostro_core::prelude::*;
use nostr_sdk::prelude::*;

use mostro_core::db::Crud;
use std::borrow::Cow;
use uuid::Uuid;

/// Publishes a dispute event to the Nostr network.
///
/// Creates and publishes a NIP-33 replaceable event containing dispute details,
/// including status, initiator (`buyer` or `seller`), and application metadata.
async fn publish_dispute_event(
    ctx: &AppContext,
    dispute: &Dispute,
    my_keys: &Keys,
    is_buyer_dispute: bool,
) -> Result<(), MostroError> {
    // Create initiator string
    let initiator = match is_buyer_dispute {
        true => "buyer",
        false => "seller",
    };

    // Create tags for the dispute event
    let tags = Tags::from_list(vec![
        // Status tag - indicates the current state of the dispute
        Tag::custom(
            TagKind::Custom(Cow::Borrowed("s")),
            vec![dispute.status.to_string()],
        ),
        // Who is the dispute creator
        Tag::custom(
            TagKind::Custom(Cow::Borrowed("initiator")),
            vec![initiator.to_string()],
        ),
        // Application identifier tag
        Tag::custom(
            TagKind::Custom(Cow::Borrowed("y")),
            create_platform_tag_values(ctx.settings().mostro.name.as_deref()),
        ),
        // Event type tag
        Tag::custom(
            TagKind::Custom(Cow::Borrowed("z")),
            vec!["dispute".to_string()],
        ),
    ]);

    // Create a new NIP-33 replaceable event (kind 38386 for disputes)
    // Empty content string as the information is in the tags
    let event = new_dispute_event(my_keys, "", dispute.id.to_string(), tags)
        .map_err(|_| MostroInternalErr(ServiceError::DisputeEventError))?;

    tracing::info!("Publishing dispute event: {:#?}", event);

    // Get nostr client from context and publish the event
    let client = ctx.nostr_client();
    match client.send_event(&event).await {
        Ok(_) => {
            tracing::info!(
                "Successfully published dispute event for dispute ID: {}",
                dispute.id
            );
            Ok(())
        }
        Err(e) => {
            tracing::error!("Failed to send dispute event: {}", e);
            Err(MostroInternalErr(ServiceError::NostrError(e.to_string())))
        }
    }
}

/// Gets information about the counterparty in a dispute.
///
/// Returns:
/// - Ok(true) if the dispute was initiated by the buyer
/// - Ok(false) if initiated by the seller
/// - Err(CantDoReason::InvalidPubkey) if the sender matches neither party
fn get_counterpart_info(sender: &str, buyer: &str, seller: &str) -> Result<bool, CantDoReason> {
    match sender {
        s if s == buyer => Ok(true),   // buyer is initiator
        s if s == seller => Ok(false), // seller is initiator
        _ => Err(CantDoReason::InvalidPubkey),
    }
}

/// Validates and retrieves an order from the database.
///
/// Checks that:
/// - The order exists
/// - The order status allows disputes (Active or FiatSent)
async fn get_valid_order(ctx: &AppContext, msg: &Message) -> Result<Order, MostroError> {
    // Try to fetch the order from the database
    let order = get_order(msg, ctx.pool()).await?;

    // Check if the order status is Active or FiatSent
    if order.check_status(Status::Active).is_err() && order.check_status(Status::FiatSent).is_err()
    {
        return Err(MostroCantDo(CantDoReason::NotAllowedByStatus));
    }

    Ok(order)
}

async fn notify_dispute_to_users(
    dispute: &Dispute,
    msg: &Message,
    order_id: Uuid,
    counterpart_pubkey: PublicKey,
    initiator_pubkey: PublicKey,
) -> Result<(), MostroError> {
    // Message to counterpart
    enqueue_order_msg(
        msg.get_inner_message_kind().request_id,
        Some(order_id),
        Action::DisputeInitiatedByPeer,
        Some(Payload::Dispute(dispute.clone().id, None)),
        counterpart_pubkey,
        None,
    )
    .await;

    // Message to dispute initiator
    enqueue_order_msg(
        msg.get_inner_message_kind().request_id,
        Some(order_id),
        Action::DisputeInitiatedByYou,
        Some(Payload::Dispute(dispute.clone().id, None)),
        initiator_pubkey,
        None,
    )
    .await;

    Ok(())
}

/// Main handler for dispute actions.
///
/// This function:
/// 1. Validates the order and dispute status
/// 2. Updates the order status
/// 3. Creates a new dispute record
/// 4. Notifies both parties
/// 5. Publishes the dispute event to the network
pub async fn dispute_action(
    ctx: &AppContext,
    msg: Message,
    event: &UnwrappedMessage,
    my_keys: &Keys,
) -> Result<(), MostroError> {
    let pool = ctx.pool();
    let order_id = if let Some(order_id) = msg.get_inner_message_kind().id {
        order_id
    } else {
        return Err(MostroCantDo(CantDoReason::NotFound));
    };
    // Check dispute for this order id is yet present.
    if find_dispute_by_order_id(pool, order_id).await.is_ok() {
        return Err(MostroInternalErr(ServiceError::DisputeAlreadyExists));
    }
    // Get and validate order
    let mut order = get_valid_order(ctx, &msg).await?;
    // Get seller and buyer pubkeys
    let (seller, buyer) = match (&order.seller_pubkey, &order.buyer_pubkey) {
        (Some(seller), Some(buyer)) => (seller.to_owned(), buyer.to_owned()),
        (None, _) => return Err(MostroInternalErr(ServiceError::InvalidPubkey)),
        (_, None) => return Err(MostroInternalErr(ServiceError::InvalidPubkey)),
    };
    // Get message sender
    let message_sender = event.sender.to_string();
    // Get counterpart info
    let is_buyer_dispute = match get_counterpart_info(&message_sender, &buyer, &seller) {
        Ok(is_buyer_dispute) => is_buyer_dispute,
        Err(cause) => return Err(MostroCantDo(cause)),
    };

    // Create new dispute record
    let dispute = Dispute::new(order_id, order.status.clone());

    // Setup dispute
    if order.setup_dispute(is_buyer_dispute).is_ok() {
        order
            .clone()
            .update(pool)
            .await
            .map_err(|cause| MostroInternalErr(ServiceError::DbAccessError(cause.to_string())))?;
    }

    // Save dispute to database
    let dispute = dispute
        .create(pool)
        .await
        .map_err(|cause| MostroInternalErr(ServiceError::DbAccessError(cause.to_string())))?;

    // Get pubkeys of initiator and counterpart
    let (initiator_pubkey, counterpart_pubkey) = if is_buyer_dispute {
        (
            &order
                .get_buyer_pubkey()
                .map_err(|_| MostroInternalErr(ServiceError::InvalidPubkey))?,
            &order
                .get_seller_pubkey()
                .map_err(|_| MostroInternalErr(ServiceError::InvalidPubkey))?,
        )
    } else {
        (
            &order
                .get_seller_pubkey()
                .map_err(|_| MostroInternalErr(ServiceError::InvalidPubkey))?,
            &order
                .get_buyer_pubkey()
                .map_err(|_| MostroInternalErr(ServiceError::InvalidPubkey))?,
        )
    };

    notify_dispute_to_users(
        &dispute,
        &msg,
        order_id,
        *counterpart_pubkey,
        *initiator_pubkey,
    )
    .await?;

    // Publish dispute event to network
    publish_dispute_event(ctx, &dispute, my_keys, is_buyer_dispute)
        .await
        .map_err(|_| MostroInternalErr(ServiceError::DisputeEventError))?;

    Ok(())
}

/// Closes a dispute after users resolve it themselves (cooperative cancel or release).
///
/// This is a best-effort operation: if the dispute update or event publishing fails,
/// errors are logged but not propagated, since the primary order operation has already
/// succeeded.
///
/// # Arguments
/// * `pool` - Database connection pool
/// * `order` - The order associated with the dispute
/// * `new_status` - The new dispute status (e.g., SellerRefunded or Settled)
/// * `my_keys` - Mostro's keys for signing the dispute event
/// * `context` - Description of the resolution context for logging (e.g., "cooperative cancel")
pub async fn close_dispute_after_user_resolution(
    ctx: &AppContext,
    order: &Order,
    new_status: DisputeStatus,
    my_keys: &Keys,
    context: &str,
) {
    let pool = ctx.pool();
    if let Ok(mut dispute) = find_dispute_by_order_id(pool, order.id).await {
        let dispute_id = dispute.id;
        dispute.status = new_status.to_string();

        if let Err(e) = dispute.update(pool).await {
            tracing::error!(
                "Failed to update dispute {} status after {}: {}",
                dispute_id,
                context,
                e
            );
        } else {
            tracing::info!(
                "Dispute {} closed automatically after {} of order {}",
                dispute_id,
                context,
                order.id
            );

            // Determine who initiated the dispute for the event tag
            let dispute_initiator = match (order.seller_dispute, order.buyer_dispute) {
                (true, false) => "seller",
                (false, true) => "buyer",
                _ => {
                    tracing::warn!(
                        "Dispute {} for order {} has inconsistent dispute flags (seller={}, buyer={}); \
                        publishing initiator as 'unknown'",
                        dispute_id,
                        order.id,
                        order.seller_dispute,
                        order.buyer_dispute,
                    );
                    "unknown"
                }
            };

            // Publish updated dispute event to Nostr so admin clients see it as resolved
            let tags = Tags::from_list(vec![
                Tag::custom(
                    TagKind::Custom(Cow::Borrowed("s")),
                    vec![new_status.to_string()],
                ),
                Tag::custom(
                    TagKind::Custom(Cow::Borrowed("initiator")),
                    vec![dispute_initiator.to_string()],
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

            match new_dispute_event(my_keys, "", dispute_id.to_string(), tags) {
                Ok(event) => {
                    let client = ctx.nostr_client();
                    tracing::info!("Publishing dispute close event: {:#?}", event);
                    if let Err(e) = client.send_event(&event).await {
                        tracing::error!("Failed to publish dispute close event: {}", e);
                    }
                }
                Err(e) => {
                    tracing::error!(
                        "Failed to create dispute close event for dispute {}: {}",
                        dispute_id,
                        e
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::context::test_utils::{test_settings, TestContextBuilder};
    use crate::config::MESSAGE_QUEUES;
    use nostr_sdk::Keys;
    use sqlx::SqlitePool;
    use std::sync::Arc;

    async fn create_test_pool() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!().run(&pool).await.unwrap();
        pool
    }

    fn build_ctx(pool: &SqlitePool) -> AppContext {
        // The publish path reads the global config (event expiration);
        // seed it once, ignoring the error when another test already did.
        let _ = crate::config::MOSTRO_CONFIG.set(test_settings());
        TestContextBuilder::new()
            .with_pool(Arc::new(pool.clone()))
            .with_settings(test_settings())
            .build()
    }

    /// Build an `UnwrappedMessage` whose trade key (rumor author / `sender`)
    /// is `sender`. The identity key is generated separately, mirroring the
    /// dual-key flow used by the cancel tests.
    fn create_event(sender: PublicKey) -> UnwrappedMessage {
        UnwrappedMessage {
            message: Message::new_order(None, Some(1), None, Action::Dispute, None),
            signature: None,
            sender,
            identity: Keys::generate().public_key(),
            created_at: Timestamp::now(),
        }
    }

    fn create_order(buyer: Option<PublicKey>, seller: Option<PublicKey>, status: Status) -> Order {
        Order {
            id: uuid::Uuid::new_v4(),
            status: status.to_string(),
            kind: mostro_core::order::Kind::Sell.to_string(),
            fiat_code: "USD".to_string(),
            creator_pubkey: seller.map(|p| p.to_string()).unwrap_or_default(),
            seller_pubkey: seller.map(|p| p.to_string()),
            buyer_pubkey: buyer.map(|p| p.to_string()),
            amount: 21_000,
            ..Default::default()
        }
    }

    fn dispute_msg_for(order_id: Option<uuid::Uuid>) -> Message {
        Message::new_order(order_id, Some(1), None, Action::Dispute, None)
    }

    #[test]
    fn get_counterpart_info_identifies_initiator_and_rejects_stranger() {
        let buyer = "buyer-pubkey";
        let seller = "seller-pubkey";

        assert_eq!(get_counterpart_info(buyer, buyer, seller), Ok(true));
        assert_eq!(get_counterpart_info(seller, buyer, seller), Ok(false));
        assert_eq!(
            get_counterpart_info("stranger", buyer, seller),
            Err(CantDoReason::InvalidPubkey)
        );
    }

    #[tokio::test]
    async fn dispute_action_without_order_id_returns_not_found() {
        let pool = create_test_pool().await;
        let ctx = build_ctx(&pool);
        let sender = Keys::generate().public_key();
        let event = create_event(sender);

        let result = dispute_action(&ctx, dispute_msg_for(None), &event, &Keys::generate()).await;

        assert!(matches!(result, Err(MostroCantDo(CantDoReason::NotFound))));
    }

    #[tokio::test]
    async fn dispute_action_with_unknown_order_id_returns_not_found() {
        let pool = create_test_pool().await;
        let ctx = build_ctx(&pool);
        let sender = Keys::generate().public_key();
        let event = create_event(sender);

        let result = dispute_action(
            &ctx,
            dispute_msg_for(Some(uuid::Uuid::new_v4())),
            &event,
            &Keys::generate(),
        )
        .await;

        assert!(matches!(result, Err(MostroCantDo(CantDoReason::NotFound))));
    }

    #[tokio::test]
    async fn dispute_action_rejects_order_that_already_has_a_dispute() {
        let pool = create_test_pool().await;
        let ctx = build_ctx(&pool);
        let buyer = Keys::generate().public_key();
        let seller = Keys::generate().public_key();

        let order = create_order(Some(buyer), Some(seller), Status::Active)
            .create(&pool)
            .await
            .unwrap();
        Dispute::new(order.id, order.status.clone())
            .create(&pool)
            .await
            .unwrap();

        let event = create_event(buyer);
        let result = dispute_action(
            &ctx,
            dispute_msg_for(Some(order.id)),
            &event,
            &Keys::generate(),
        )
        .await;

        assert!(matches!(
            result,
            Err(MostroInternalErr(ServiceError::DisputeAlreadyExists))
        ));
    }

    #[tokio::test]
    async fn dispute_action_rejects_order_with_non_disputable_status() {
        let pool = create_test_pool().await;
        let ctx = build_ctx(&pool);
        let buyer = Keys::generate().public_key();
        let seller = Keys::generate().public_key();

        let order = create_order(Some(buyer), Some(seller), Status::Pending)
            .create(&pool)
            .await
            .unwrap();

        let event = create_event(buyer);
        let result = dispute_action(
            &ctx,
            dispute_msg_for(Some(order.id)),
            &event,
            &Keys::generate(),
        )
        .await;

        assert!(matches!(
            result,
            Err(MostroCantDo(CantDoReason::NotAllowedByStatus))
        ));
    }

    #[tokio::test]
    async fn dispute_action_rejects_order_missing_seller_pubkey() {
        let pool = create_test_pool().await;
        let ctx = build_ctx(&pool);
        let buyer = Keys::generate().public_key();

        let order = create_order(Some(buyer), None, Status::Active)
            .create(&pool)
            .await
            .unwrap();

        let event = create_event(buyer);
        let result = dispute_action(
            &ctx,
            dispute_msg_for(Some(order.id)),
            &event,
            &Keys::generate(),
        )
        .await;

        assert!(matches!(
            result,
            Err(MostroInternalErr(ServiceError::InvalidPubkey))
        ));
    }

    #[tokio::test]
    async fn dispute_action_rejects_order_missing_buyer_pubkey() {
        let pool = create_test_pool().await;
        let ctx = build_ctx(&pool);
        let seller = Keys::generate().public_key();

        let order = create_order(None, Some(seller), Status::Active)
            .create(&pool)
            .await
            .unwrap();

        let event = create_event(seller);
        let result = dispute_action(
            &ctx,
            dispute_msg_for(Some(order.id)),
            &event,
            &Keys::generate(),
        )
        .await;

        assert!(matches!(
            result,
            Err(MostroInternalErr(ServiceError::InvalidPubkey))
        ));
    }

    #[tokio::test]
    async fn dispute_action_rejects_sender_that_is_not_a_party() {
        let pool = create_test_pool().await;
        let ctx = build_ctx(&pool);
        let buyer = Keys::generate().public_key();
        let seller = Keys::generate().public_key();

        let order = create_order(Some(buyer), Some(seller), Status::Active)
            .create(&pool)
            .await
            .unwrap();

        let intruder = Keys::generate().public_key();
        let event = create_event(intruder);
        let result = dispute_action(
            &ctx,
            dispute_msg_for(Some(order.id)),
            &event,
            &Keys::generate(),
        )
        .await;

        assert!(matches!(
            result,
            Err(MostroCantDo(CantDoReason::InvalidPubkey))
        ));
    }

    /// Full buyer-initiated flow on an `Active` order. All DB side effects
    /// (order flags/status, dispute row) and both queue notifications happen
    /// before the final Nostr publish, which fails offline (default client
    /// with no relays), so the handler ends in `DisputeEventError`. The
    /// publish-success branch of `publish_dispute_event` is unreachable in
    /// unit tests.
    #[tokio::test]
    async fn dispute_action_buyer_initiated_flow_persists_dispute_and_notifies() {
        let pool = create_test_pool().await;
        let ctx = build_ctx(&pool);
        let buyer = Keys::generate().public_key();
        let seller = Keys::generate().public_key();

        let order = create_order(Some(buyer), Some(seller), Status::Active)
            .create(&pool)
            .await
            .unwrap();

        let event = create_event(buyer);
        let result = dispute_action(
            &ctx,
            dispute_msg_for(Some(order.id)),
            &event,
            &Keys::generate(),
        )
        .await;

        assert!(matches!(
            result,
            Err(MostroInternalErr(ServiceError::DisputeEventError))
        ));

        // Order flags and status were persisted before the publish failed
        let stored_order = Order::by_id(&pool, order.id).await.unwrap().unwrap();
        assert!(stored_order.buyer_dispute);
        assert!(!stored_order.seller_dispute);
        assert_eq!(stored_order.status, Status::Dispute.to_string());

        // Dispute row was created preserving the previous order status
        let dispute = find_dispute_by_order_id(&pool, order.id).await.unwrap();
        assert_eq!(dispute.status, DisputeStatus::Initiated.to_string());
        assert_eq!(dispute.order_previous_status, Status::Active.to_string());

        // Both parties were notified (queue is global; filter by order id)
        let queue = MESSAGE_QUEUES.queue_order_msg.read().await;
        let notifications: Vec<_> = queue
            .iter()
            .filter(|(m, _)| m.get_inner_message_kind().id == Some(order.id))
            .collect();
        assert_eq!(notifications.len(), 2);
        assert!(notifications.iter().any(|(m, dest)| {
            m.get_inner_message_kind().action == Action::DisputeInitiatedByPeer && *dest == seller
        }));
        assert!(notifications.iter().any(|(m, dest)| {
            m.get_inner_message_kind().action == Action::DisputeInitiatedByYou && *dest == buyer
        }));
    }

    #[tokio::test]
    async fn dispute_action_seller_initiated_flow_on_fiat_sent_order() {
        let pool = create_test_pool().await;
        let ctx = build_ctx(&pool);
        let buyer = Keys::generate().public_key();
        let seller = Keys::generate().public_key();

        let order = create_order(Some(buyer), Some(seller), Status::FiatSent)
            .create(&pool)
            .await
            .unwrap();

        let event = create_event(seller);
        let result = dispute_action(
            &ctx,
            dispute_msg_for(Some(order.id)),
            &event,
            &Keys::generate(),
        )
        .await;

        assert!(matches!(
            result,
            Err(MostroInternalErr(ServiceError::DisputeEventError))
        ));

        let stored_order = Order::by_id(&pool, order.id).await.unwrap().unwrap();
        assert!(stored_order.seller_dispute);
        assert!(!stored_order.buyer_dispute);
        assert_eq!(stored_order.status, Status::Dispute.to_string());

        let dispute = find_dispute_by_order_id(&pool, order.id).await.unwrap();
        assert_eq!(dispute.order_previous_status, Status::FiatSent.to_string());

        let queue = MESSAGE_QUEUES.queue_order_msg.read().await;
        let notifications: Vec<_> = queue
            .iter()
            .filter(|(m, _)| m.get_inner_message_kind().id == Some(order.id))
            .collect();
        assert_eq!(notifications.len(), 2);
        assert!(notifications.iter().any(|(m, dest)| {
            m.get_inner_message_kind().action == Action::DisputeInitiatedByPeer && *dest == buyer
        }));
        assert!(notifications.iter().any(|(m, dest)| {
            m.get_inner_message_kind().action == Action::DisputeInitiatedByYou && *dest == seller
        }));
    }

    #[tokio::test]
    async fn close_dispute_after_user_resolution_is_noop_without_dispute_row() {
        let pool = create_test_pool().await;
        let ctx = build_ctx(&pool);
        let buyer = Keys::generate().public_key();
        let seller = Keys::generate().public_key();
        let order = create_order(Some(buyer), Some(seller), Status::Active);

        // No dispute row exists: must be a silent no-op
        close_dispute_after_user_resolution(
            &ctx,
            &order,
            DisputeStatus::Settled,
            &Keys::generate(),
            "release",
        )
        .await;

        assert!(find_dispute_by_order_id(&pool, order.id).await.is_err());
    }

    /// Consistent flags (`seller_dispute` only) resolve to the "seller"
    /// initiator branch; the dispute row is updated even though the final
    /// event publish fails offline (error is only logged).
    #[tokio::test]
    async fn close_dispute_after_user_resolution_updates_dispute_status() {
        let pool = create_test_pool().await;
        let ctx = build_ctx(&pool);
        let buyer = Keys::generate().public_key();
        let seller = Keys::generate().public_key();

        let mut order = create_order(Some(buyer), Some(seller), Status::Dispute);
        order.seller_dispute = true;
        let order = order.create(&pool).await.unwrap();
        Dispute::new(order.id, Status::Active.to_string())
            .create(&pool)
            .await
            .unwrap();

        close_dispute_after_user_resolution(
            &ctx,
            &order,
            DisputeStatus::SellerRefunded,
            &Keys::generate(),
            "cooperative cancel",
        )
        .await;

        let dispute = find_dispute_by_order_id(&pool, order.id).await.unwrap();
        assert_eq!(dispute.status, DisputeStatus::SellerRefunded.to_string());
    }

    /// Inconsistent flags (both unset) fall into the "unknown" initiator
    /// branch; the dispute status update must still be persisted.
    #[tokio::test]
    async fn close_dispute_after_user_resolution_handles_inconsistent_flags() {
        let pool = create_test_pool().await;
        let ctx = build_ctx(&pool);
        let buyer = Keys::generate().public_key();
        let seller = Keys::generate().public_key();

        // Neither dispute flag set: inconsistent with an existing dispute row
        let order = create_order(Some(buyer), Some(seller), Status::Dispute)
            .create(&pool)
            .await
            .unwrap();
        Dispute::new(order.id, Status::Active.to_string())
            .create(&pool)
            .await
            .unwrap();

        close_dispute_after_user_resolution(
            &ctx,
            &order,
            DisputeStatus::Settled,
            &Keys::generate(),
            "release",
        )
        .await;

        let dispute = find_dispute_by_order_id(&pool, order.id).await.unwrap();
        assert_eq!(dispute.status, DisputeStatus::Settled.to_string());
    }
}
