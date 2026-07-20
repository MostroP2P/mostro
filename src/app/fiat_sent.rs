use crate::app::context::AppContext;
use crate::util::{enqueue_order_msg, get_order, update_order_event};
use mostro_core::db::Crud;
use mostro_core::prelude::*;
use nostr_sdk::prelude::*;

// Handle fiat sent action
pub async fn fiat_sent_action(
    ctx: &AppContext,
    msg: Message,
    event: &UnwrappedMessage,
    my_keys: &Keys,
) -> Result<(), MostroError> {
    let pool = ctx.pool();
    // Get order
    let order = get_order(&msg, pool).await?;

    // Check if the order status is active
    if let Err(cause) = order.check_status(Status::Active) {
        return Err(MostroCantDo(cause));
    }

    // Check if the pubkey is the buyer pubkey - Only the buyer can send fiat
    // if someone else tries to send fiat, we return an error
    if order.get_buyer_pubkey().ok() != Some(event.sender) {
        return Err(MostroCantDo(CantDoReason::InvalidPubkey));
    }

    // Get next trade key
    let next_trade = msg
        .get_inner_message_kind()
        .get_next_trade_key()
        .map_err(MostroInternalErr)?;

    // We publish a new replaceable kind nostr event with the status updated
    // and update on local database the status and new event id
    let mut order_updated = update_order_event(my_keys, Status::FiatSent, &order)
        .await
        .map_err(|e| MostroError::MostroInternalErr(ServiceError::NostrError(e.to_string())))?;

    let seller_pubkey = order.get_seller_pubkey().map_err(MostroInternalErr)?;

    // Create peer
    let peer = Peer {
        pubkey: event.sender.to_string(),
        reputation: None,
    };

    // Notify seller that fiat was sent
    enqueue_order_msg(
        None,
        Some(order_updated.id),
        Action::FiatSentOk,
        Some(Payload::Peer(peer)),
        seller_pubkey,
        None,
    )
    .await;
    // We send a message to buyer to wait
    let peer = Peer {
        pubkey: seller_pubkey.to_string(),
        reputation: None,
    };

    // Notify buyer that fiat was sent
    enqueue_order_msg(
        msg.get_inner_message_kind().request_id,
        Some(order_updated.id),
        Action::FiatSentOk,
        Some(Payload::Peer(peer)),
        event.sender,
        None,
    )
    .await;

    // If this is a range order, we need to update next trade fields
    if order.is_range_order() {
        // Update next trade fields only when the buyer is the maker of a range order
        // These fields will be used to create the next child order in the range
        if let Some((pubkey, index)) = next_trade {
            order_updated.next_trade_pubkey = Some(pubkey);
            order_updated.next_trade_index = Some(index as i64);
        }
    }

    // Update order
    order_updated
        .update(pool)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::context::test_utils::{test_settings, TestContextBuilder};
    use crate::app::context::AppContext;
    use crate::config::{MESSAGE_QUEUES, MOSTRO_CONFIG};
    use nostr_sdk::{Keys, Timestamp};
    use sqlx::SqlitePool;
    use std::sync::Arc;

    /// The `MOSTRO_CONFIG` OnceLock is process-global: set it to the shared
    /// `test_settings()` defaults (idempotent across concurrent tests).
    fn init_global_config() {
        let _ = MOSTRO_CONFIG.set(test_settings());
    }

    async fn create_test_pool() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!().run(&pool).await.unwrap();
        pool
    }

    fn build_ctx(pool: &SqlitePool) -> AppContext {
        TestContextBuilder::new()
            .with_pool(Arc::new(pool.clone()))
            .with_settings(test_settings())
            .build()
    }

    /// Build an `UnwrappedMessage` whose trade key (rumor author / `sender`)
    /// is `pubkey`, mirroring the fixture used by the cancel handler tests.
    fn create_unwrapped_message_with_pubkey(pubkey: PublicKey) -> UnwrappedMessage {
        UnwrappedMessage {
            message: Message::Order(MessageKind::new(
                Some(uuid::Uuid::new_v4()),
                Some(1),
                None,
                Action::FiatSent,
                None,
            )),
            signature: None,
            sender: pubkey,
            identity: Keys::generate().public_key(),
            created_at: Timestamp::now(),
        }
    }

    fn active_sell_order(seller: PublicKey, buyer: PublicKey) -> Order {
        Order {
            id: uuid::Uuid::new_v4(),
            status: Status::Active.to_string(),
            kind: mostro_core::order::Kind::Sell.to_string(),
            fiat_code: "USD".to_string(),
            creator_pubkey: seller.to_string(),
            seller_pubkey: Some(seller.to_string()),
            master_seller_pubkey: Some(seller.to_string()),
            buyer_pubkey: Some(buyer.to_string()),
            master_buyer_pubkey: Some(buyer.to_string()),
            amount: 21_000,
            fee: 21,
            fiat_amount: 40,
            ..Default::default()
        }
    }

    fn fiat_sent_message(order_id: uuid::Uuid, payload: Option<Payload>) -> Message {
        Message::new_order(Some(order_id), Some(1), None, Action::FiatSent, payload)
    }

    /// Actions queued on the process-global order queue for a given order id.
    /// The queue is shared across concurrently running tests, so assertions
    /// must always filter by our own order id.
    async fn queued_actions_for(order_id: uuid::Uuid) -> Vec<Action> {
        MESSAGE_QUEUES
            .queue_order_msg
            .read()
            .await
            .iter()
            .filter(|(msg, _)| msg.get_inner_message_kind().id == Some(order_id))
            .map(|(msg, _)| msg.get_inner_message_kind().action.clone())
            .collect()
    }

    #[tokio::test]
    async fn fiat_sent_action_rejects_order_that_is_not_active() {
        // Arrange
        let pool = create_test_pool().await;
        let ctx = build_ctx(&pool);
        let seller = Keys::generate().public_key();
        let buyer = Keys::generate().public_key();
        let mut order = active_sell_order(seller, buyer);
        order.status = Status::Pending.to_string();
        let order = order.create(&pool).await.unwrap();
        let event = create_unwrapped_message_with_pubkey(buyer);
        let msg = fiat_sent_message(order.id, None);
        let my_keys = Keys::generate();

        // Act
        let result = fiat_sent_action(&ctx, msg, &event, &my_keys).await;

        // Assert
        assert!(matches!(result, Err(MostroCantDo(_))));
    }

    #[tokio::test]
    async fn fiat_sent_action_rejects_sender_that_is_not_the_buyer() {
        // Arrange: the seller (not the buyer) tries to mark fiat as sent.
        let pool = create_test_pool().await;
        let ctx = build_ctx(&pool);
        let seller = Keys::generate().public_key();
        let buyer = Keys::generate().public_key();
        let order = active_sell_order(seller, buyer)
            .create(&pool)
            .await
            .unwrap();
        let event = create_unwrapped_message_with_pubkey(seller);
        let msg = fiat_sent_message(order.id, None);
        let my_keys = Keys::generate();

        // Act
        let result = fiat_sent_action(&ctx, msg, &event, &my_keys).await;

        // Assert
        assert!(matches!(
            result,
            Err(MostroCantDo(CantDoReason::InvalidPubkey))
        ));
    }

    #[tokio::test]
    async fn fiat_sent_action_rejects_invalid_next_trade_payload() {
        // Arrange: a payload that is not NextTrade makes key extraction fail.
        let pool = create_test_pool().await;
        let ctx = build_ctx(&pool);
        let seller = Keys::generate().public_key();
        let buyer = Keys::generate().public_key();
        let order = active_sell_order(seller, buyer)
            .create(&pool)
            .await
            .unwrap();
        let event = create_unwrapped_message_with_pubkey(buyer);
        let msg = fiat_sent_message(order.id, Some(Payload::RatingUser(5)));
        let my_keys = Keys::generate();

        // Act
        let result = fiat_sent_action(&ctx, msg, &event, &my_keys).await;

        // Assert
        assert!(matches!(
            result,
            Err(MostroInternalErr(ServiceError::InvalidPayload))
        ));
    }

    #[tokio::test]
    async fn fiat_sent_action_marks_order_fiat_sent_and_notifies_both_peers() {
        // Arrange
        init_global_config();
        let pool = create_test_pool().await;
        let ctx = build_ctx(&pool);
        let seller = Keys::generate().public_key();
        let buyer = Keys::generate().public_key();
        let order = active_sell_order(seller, buyer)
            .create(&pool)
            .await
            .unwrap();
        let event = create_unwrapped_message_with_pubkey(buyer);
        let msg = fiat_sent_message(order.id, None);
        let my_keys = Keys::generate();

        // Act
        let result = fiat_sent_action(&ctx, msg, &event, &my_keys).await;

        // Assert: status persisted, both peers got FiatSentOk, no next trade.
        assert!(result.is_ok());
        let db_order = Order::by_id(&pool, order.id).await.unwrap().unwrap();
        assert_eq!(db_order.status, Status::FiatSent.to_string());
        assert_eq!(db_order.next_trade_pubkey, None);
        assert_eq!(db_order.next_trade_index, None);
        let fiat_sent_oks = queued_actions_for(order.id)
            .await
            .into_iter()
            .filter(|action| *action == Action::FiatSentOk)
            .count();
        assert_eq!(fiat_sent_oks, 2);
    }

    #[tokio::test]
    async fn fiat_sent_action_stores_next_trade_for_range_orders() {
        // Arrange: range order plus a NextTrade payload from the buyer-maker.
        init_global_config();
        let pool = create_test_pool().await;
        let ctx = build_ctx(&pool);
        let seller = Keys::generate().public_key();
        let buyer = Keys::generate().public_key();
        let next_trade = Keys::generate().public_key();
        let mut order = active_sell_order(seller, buyer);
        order.max_amount = Some(100);
        order.min_amount = Some(10);
        let order = order.create(&pool).await.unwrap();
        let event = create_unwrapped_message_with_pubkey(buyer);
        let msg = fiat_sent_message(
            order.id,
            Some(Payload::NextTrade(next_trade.to_string(), 7)),
        );
        let my_keys = Keys::generate();

        // Act
        let result = fiat_sent_action(&ctx, msg, &event, &my_keys).await;

        // Assert: next trade fields persisted alongside the status change.
        assert!(result.is_ok());
        let db_order = Order::by_id(&pool, order.id).await.unwrap().unwrap();
        assert_eq!(db_order.status, Status::FiatSent.to_string());
        assert_eq!(db_order.next_trade_pubkey, Some(next_trade.to_string()));
        assert_eq!(db_order.next_trade_index, Some(7));
    }
}
