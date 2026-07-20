use crate::app::context::AppContext;
use crate::util::{enqueue_order_msg, get_user_orders_by_id};
use mostro_core::prelude::*;
use nostr_sdk::prelude::*;

// Handle orders action
pub async fn orders_action(
    ctx: &AppContext,
    msg: Message,
    event: &UnwrappedMessage,
) -> Result<(), MostroError> {
    let pool = ctx.pool();
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

    let mostro_settings = &ctx.settings().mostro;
    if ids.len() > mostro_settings.max_orders_per_response as usize {
        return Err(MostroCantDo(CantDoReason::TooManyRequests));
    }

    // Get orders
    let orders = get_user_orders_by_id(pool, ids, &event.identity.to_string()).await?;
    if orders.is_empty() {
        return Err(MostroCantDo(CantDoReason::NotFound));
    }
    let small_orders = orders
        .into_iter()
        .map(|order| {
            let mut small = SmallOrder::from(order);
            // Clear buyer_invoice to avoid leaking buyer's payment info
            small.buyer_invoice = None;
            small
        })
        .collect::<Vec<SmallOrder>>();
    let response_payload = Payload::Orders(small_orders);
    // Enqueue response message
    enqueue_order_msg(
        msg.get_inner_message_kind().request_id,
        None,
        Action::Orders,
        Some(response_payload),
        event.sender,
        None,
    )
    .await;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::context::test_utils::{test_settings, TestContextBuilder};
    use mostro_core::db::Crud;
    use nostr_sdk::{Keys, Timestamp};
    use sqlx::SqlitePool;
    use std::sync::Arc;
    use uuid::Uuid;

    async fn setup_ctx() -> AppContext {
        let pool = Arc::new(SqlitePool::connect("sqlite::memory:").await.unwrap());
        sqlx::migrate!("./migrations")
            .run(pool.as_ref())
            .await
            .unwrap();
        TestContextBuilder::new()
            .with_pool(pool)
            .with_settings(test_settings())
            .build()
    }

    /// `sender` is the trade key the response is addressed to; `identity`
    /// is the master key ownership checks run against.
    fn orders_event(sender: PublicKey, identity: PublicKey) -> UnwrappedMessage {
        UnwrappedMessage {
            message: Message::Order(MessageKind::new(None, Some(1), None, Action::Orders, None)),
            signature: None,
            sender,
            identity,
            created_at: Timestamp::now(),
        }
    }

    fn orders_msg(payload: Option<Payload>) -> Message {
        Message::new_order(None, Some(1), None, Action::Orders, payload)
    }

    fn base_order() -> Order {
        Order {
            id: Uuid::new_v4(),
            status: Status::Pending.to_string(),
            kind: mostro_core::order::Kind::Sell.to_string(),
            fiat_code: "USD".to_string(),
            creator_pubkey: Keys::generate().public_key().to_string(),
            amount: 10_000,
            fee: 10,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn orders_action_rejects_missing_or_non_ids_payload() {
        let ctx = setup_ctx().await;
        let sender = Keys::generate().public_key();
        let identity = Keys::generate().public_key();
        let event = orders_event(sender, identity);

        let result = orders_action(&ctx, orders_msg(None), &event).await;

        assert!(matches!(
            result,
            Err(MostroCantDo(CantDoReason::InvalidParameters))
        ));
    }

    #[tokio::test]
    async fn orders_action_rejects_empty_ids_list() {
        let ctx = setup_ctx().await;
        let sender = Keys::generate().public_key();
        let identity = Keys::generate().public_key();
        let event = orders_event(sender, identity);

        let result = orders_action(&ctx, orders_msg(Some(Payload::Ids(vec![]))), &event).await;

        assert!(matches!(
            result,
            Err(MostroCantDo(CantDoReason::InvalidParameters))
        ));
    }

    #[tokio::test]
    async fn orders_action_rejects_more_ids_than_configured_maximum() {
        let ctx = setup_ctx().await;
        let sender = Keys::generate().public_key();
        let identity = Keys::generate().public_key();
        let event = orders_event(sender, identity);

        let too_many = ctx.settings().mostro.max_orders_per_response as usize + 1;
        let ids: Vec<Uuid> = (0..too_many).map(|_| Uuid::new_v4()).collect();

        let result = orders_action(&ctx, orders_msg(Some(Payload::Ids(ids))), &event).await;

        assert!(matches!(
            result,
            Err(MostroCantDo(CantDoReason::TooManyRequests))
        ));
    }

    #[tokio::test]
    async fn orders_action_returns_not_found_for_unknown_or_foreign_orders() {
        let ctx = setup_ctx().await;
        let sender = Keys::generate().public_key();
        let identity = Keys::generate().public_key();
        let event = orders_event(sender, identity);

        // An order owned by a completely different master key must be
        // invisible to this identity.
        let mut foreign_order = base_order();
        foreign_order.master_buyer_pubkey = Some(Keys::generate().public_key().to_string());
        let foreign_order = foreign_order.create(ctx.pool()).await.unwrap();

        let ids = vec![Uuid::new_v4(), foreign_order.id];
        let result = orders_action(&ctx, orders_msg(Some(Payload::Ids(ids))), &event).await;

        assert!(matches!(result, Err(MostroCantDo(CantDoReason::NotFound))));
    }

    #[tokio::test]
    async fn orders_action_returns_user_orders_and_clears_buyer_invoice() {
        let ctx = setup_ctx().await;
        let sender = Keys::generate().public_key();
        let identity = Keys::generate().public_key();
        let event = orders_event(sender, identity);

        let mut as_buyer = base_order();
        as_buyer.master_buyer_pubkey = Some(identity.to_string());
        as_buyer.buyer_invoice = Some("lnbc1-private-invoice".to_string());
        let as_buyer = as_buyer.create(ctx.pool()).await.unwrap();

        let mut as_seller = base_order();
        as_seller.master_seller_pubkey = Some(identity.to_string());
        let as_seller = as_seller.create(ctx.pool()).await.unwrap();

        let ids = vec![as_buyer.id, as_seller.id];
        let result = orders_action(&ctx, orders_msg(Some(Payload::Ids(ids))), &event).await;
        assert!(result.is_ok(), "owned ids must resolve: {result:?}");

        // The response goes to the process-global queue; filter by this
        // test's unique sender key to stay isolated from parallel tests.
        let queued: Vec<Message> = crate::config::MESSAGE_QUEUES
            .queue_order_msg
            .read()
            .await
            .iter()
            .filter(|(_, pk)| *pk == sender)
            .map(|(m, _)| m.clone())
            .collect();
        assert_eq!(queued.len(), 1, "exactly one response for this sender");

        let kind = queued[0].get_inner_message_kind();
        assert_eq!(kind.action, Action::Orders);
        match kind.get_payload() {
            Some(Payload::Orders(small_orders)) => {
                let returned_ids: Vec<Option<Uuid>> = small_orders.iter().map(|o| o.id).collect();
                assert_eq!(returned_ids, vec![Some(as_buyer.id), Some(as_seller.id)]);
                assert!(
                    small_orders.iter().all(|o| o.buyer_invoice.is_none()),
                    "buyer_invoice must be stripped from the response"
                );
            }
            other => panic!("expected Payload::Orders, got {other:?}"),
        }
    }
}
