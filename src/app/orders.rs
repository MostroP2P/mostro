use crate::util::{enqueue_order_msg, get_user_orders_by_id};
use crate::config::settings::Settings;
use mostro_core::prelude::*;
use nostr_sdk::prelude::*;
use sqlx::{Pool, Sqlite};

// Handle orders action
pub async fn orders_action(
    msg: Message,
    event: &UnwrappedGift,
    pool: &Pool<Sqlite>,
) -> Result<(), MostroError> {
    // Get order
    let payload = msg.get_inner_message_kind().get_payload();

    let ids = match payload {
        Some(Payload::Ids(ids)) => ids,
        _ => return Err(MostroCantDo(CantDoReason::InvalidParameters)),
    };

    // Return an error to the caller if the payload contains no usable identifiers
    if ids.is_empty() {
        return Err(MostroCantDo(CantDoReason::InvalidParameters));
    }

    let mostro_settings = Settings::get_mostro();
    if ids.len() > mostro_settings.max_orders_per_response as usize {
        return Err(MostroCantDo(CantDoReason::TooManyRequests));
    }

    // Get orders
    let orders = get_user_orders_by_id(pool, ids, &event.rumor.pubkey.to_string()).await?;
    if orders.is_empty() {
        return Err(MostroCantDo(CantDoReason::NotFound));
    }
    let small_orders = orders
        .into_iter()
        .map(SmallOrder::from)
        .collect::<Vec<SmallOrder>>();
    let response_payload = Payload::Orders(small_orders);
    // Enqueue response message
    enqueue_order_msg(
        msg.get_inner_message_kind().request_id,
        None,
        Action::Orders,
        Some(response_payload),
        event.rumor.pubkey,
        None,
    )
    .await;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MESSAGE_QUEUES;
    use mostro_core::message::MessageKind;
    use nostr_sdk::{prelude::PublicKey, Keys, Kind as NostrKind, Timestamp, UnsignedEvent};
    use sqlx::sqlite::SqlitePoolOptions;
    use sqlx::SqlitePool;
    use uuid::Uuid;

    async fn setup_orders_pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect(":memory:")
            .await
            .unwrap();

        sqlx::query(include_str!("../../migrations/20221222153301_orders.sql"))
            .execute(&pool)
            .await
            .unwrap();

        pool
    }

    async fn insert_order(
        pool: &SqlitePool,
        id: Uuid,
        buyer_pubkey: Option<&str>,
        seller_pubkey: Option<&str>,
        creator_pubkey: &str,
    ) {
        sqlx::query(
            r#"
            INSERT INTO orders (
                id,
                kind,
                event_id,
                creator_pubkey,
                status,
                premium,
                payment_method,
                amount,
                fiat_code,
                fiat_amount,
                created_at,
                expires_at,
                buyer_pubkey,
                seller_pubkey
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        "#,
        )
        .bind(id)
        .bind("buy")
        .bind(id.simple().to_string())
        .bind(creator_pubkey)
        .bind("active")
        .bind(0_i64)
        .bind("ln")
        .bind(1_000_i64)
        .bind("USD")
        .bind(1_000_i64)
        .bind(1_000_i64)
        .bind(2_000_i64)
        .bind(buyer_pubkey)
        .bind(seller_pubkey)
        .execute(pool)
        .await
        .unwrap();
    }

    fn build_event(rumor_pubkey: PublicKey, sender_pubkey: PublicKey) -> UnwrappedGift {
        let unsigned_event = UnsignedEvent::new(
            rumor_pubkey,
            Timestamp::now(),
            NostrKind::GiftWrap,
            Vec::new(),
            "",
        );

        UnwrappedGift {
            sender: sender_pubkey,
            rumor: unsigned_event,
        }
    }

    async fn clear_order_queue() {
        MESSAGE_QUEUES.queue_order_msg.write().await.clear();
    }

    #[tokio::test]
    async fn test_orders_action_returns_matching_orders() {
        let pool = setup_orders_pool().await;
        clear_order_queue().await;

        let user_keys = Keys::generate();
        let sender_keys = Keys::generate();
        let user_pubkey = user_keys.public_key();
        let user_pubkey_str = user_pubkey.to_string();

        let first_id = Uuid::new_v4();
        let second_id = Uuid::new_v4();

        insert_order(
            &pool,
            first_id,
            Some(user_pubkey_str.as_str()),
            None,
            user_pubkey_str.as_str(),
        )
        .await;
        insert_order(
            &pool,
            second_id,
            None,
            Some(user_pubkey_str.as_str()),
            user_pubkey_str.as_str(),
        )
        .await;

        let msg = Message::Order(MessageKind::new(
            Some(Uuid::new_v4()),
            Some(42),
            None,
            Action::Orders,
            Some(Payload::Ids(vec![first_id, second_id])),
        ));

        let event = build_event(user_pubkey, sender_keys.public_key());

        let result = orders_action(msg, &event, &pool).await;
        assert!(result.is_ok());

        let queue = MESSAGE_QUEUES.queue_order_msg.read().await;
        assert_eq!(queue.len(), 1);

        let (response, destination) = queue.last().unwrap();
        assert_eq!(*destination, user_pubkey);

        match response.get_inner_message_kind().payload.as_ref() {
            Some(Payload::Orders(orders)) => {
                assert_eq!(orders.len(), 2);
                assert_eq!(orders[0].id, Some(first_id));
                assert_eq!(orders[1].id, Some(second_id));
            }
            other => panic!("Expected orders payload, got {other:?}"),
        }

        // Clean up the queue for other tests
        clear_order_queue().await;
    }

    #[tokio::test]
    async fn test_orders_action_rejects_invalid_payload() {
        let pool = setup_orders_pool().await;
        clear_order_queue().await;

        let user_keys = Keys::generate();
        let sender_keys = Keys::generate();
        let user_pubkey = user_keys.public_key();

        let msg = Message::Order(MessageKind::new(
            Some(Uuid::new_v4()),
            Some(24),
            None,
            Action::Orders,
            Some(Payload::Amount(1_000)),
        ));

        let event = build_event(user_pubkey, sender_keys.public_key());

        let err = orders_action(msg, &event, &pool)
            .await
            .expect_err("orders_action should fail with invalid payload");

        match err {
            MostroCantDo(reason) => assert_eq!(reason, CantDoReason::InvalidParameters),
            other => panic!("Unexpected error: {other:?}"),
        }

        let queue = MESSAGE_QUEUES.queue_order_msg.read().await;
        assert!(queue.is_empty());

        // Clean up the queue for other tests
        clear_order_queue().await;
    }

    #[tokio::test]
    async fn test_orders_action_returns_not_found_for_missing_orders() {
        let pool = setup_orders_pool().await;
        clear_order_queue().await;

        let user_keys = Keys::generate();
        let sender_keys = Keys::generate();
        let other_keys = Keys::generate();
        let user_pubkey = user_keys.public_key();
        let other_pubkey = other_keys.public_key();
        let other_pubkey_str = other_pubkey.to_string();

        let missing_id = Uuid::new_v4();
        // Insert order that belongs to a different pubkey to ensure filtering removes it
        insert_order(
            &pool,
            missing_id,
            Some(other_pubkey_str.as_str()),
            None,
            other_pubkey_str.as_str(),
        )
        .await;

        let msg = Message::Order(MessageKind::new(
            Some(Uuid::new_v4()),
            Some(11),
            None,
            Action::Orders,
            Some(Payload::Ids(vec![missing_id])),
        ));

        let event = build_event(user_pubkey, sender_keys.public_key());

        let err = orders_action(msg, &event, &pool)
            .await
            .expect_err("orders_action should fail when user owns no orders");

        match err {
            MostroCantDo(reason) => assert_eq!(reason, CantDoReason::NotFound),
            other => panic!("Unexpected error: {other:?}"),
        }

        let queue = MESSAGE_QUEUES.queue_order_msg.read().await;
        assert!(queue.is_empty());

        // Clean up the queue for other tests
        clear_order_queue().await;
    }

    #[tokio::test]
    async fn test_orders_action_rejects_empty_ids() {
        let pool = setup_orders_pool().await;
        clear_order_queue().await;

        let user_keys = Keys::generate();
        let sender_keys = Keys::generate();
        let user_pubkey = user_keys.public_key();

        let msg = Message::Order(MessageKind::new(
            Some(Uuid::new_v4()),
            Some(99),
            None,
            Action::Orders,
            Some(Payload::Ids(vec![])),
        ));

        let event = build_event(user_pubkey, sender_keys.public_key());

        let err = orders_action(msg, &event, &pool)
            .await
            .expect_err("orders_action should fail with empty ids");

        match err {
            MostroCantDo(reason) => assert_eq!(reason, CantDoReason::InvalidParameters),
            other => panic!("Unexpected error: {other:?}"),
        }

        clear_order_queue().await;
    }
}
