use crate::app::context::AppContext;
use crate::util::{enqueue_order_msg, get_order};

use mostro_core::db::Crud;
use mostro_core::prelude::*;
use nostr_sdk::prelude::*;

pub async fn trade_pubkey_action(
    ctx: &AppContext,
    msg: Message,
    event: &UnwrappedMessage,
) -> Result<(), MostroError> {
    let pool = ctx.pool();
    // Get request id
    let request_id = msg.get_inner_message_kind().request_id;
    // Get order
    let mut order = get_order(&msg, pool).await?;

    // Phase 1.5: accept both `Pending` and `WaitingTakerBond` as
    // pre-trade entry points. The trade pubkey is a maker-only piece of
    // state, unaffected by which (if any) prospective taker happens to
    // be mid-bond; gating on `Pending` alone would block legitimate
    // maker rotation during the bond window.
    if order.check_status(Status::Pending).is_err()
        && order.check_status(Status::WaitingTakerBond).is_err()
    {
        return Err(MostroCantDo(CantDoReason::InvalidOrderStatus));
    }

    // Get master keys (already plaintext after phase-3/4 encryption migration)
    let (master_buyer_key, master_seller_key) = if order.master_buyer_pubkey.is_some() {
        let master_buyer_key = order
            .get_master_buyer_pubkey()
            .map_err(MostroInternalErr)?
            .to_string();
        (Some(master_buyer_key), None)
    } else {
        let master_seller_key = order
            .get_master_seller_pubkey()
            .map_err(MostroInternalErr)?
            .to_string();
        (None, Some(master_seller_key))
    };

    match (master_buyer_key, master_seller_key) {
        (Some(master_buyer_pubkey), _) if master_buyer_pubkey == event.identity.to_string() => {
            order.buyer_pubkey = Some(event.sender.to_string());
        }
        (_, Some(master_seller_pubkey)) if master_seller_pubkey == event.identity.to_string() => {
            order.seller_pubkey = Some(event.sender.to_string());
        }
        _ => return Err(MostroInternalErr(ServiceError::InvalidPubkey)),
    };
    order.creator_pubkey = event.sender.to_string();

    // We a message to the seller
    enqueue_order_msg(
        request_id,
        Some(order.id),
        Action::TradePubkey,
        None,
        event.sender,
        None,
    )
    .await;

    order
        .update(pool)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::context::test_utils::{test_settings, TestContextBuilder};
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

    /// `sender` is the fresh trade key being rotated in; `identity` is the
    /// master key the ownership check runs against.
    fn trade_pubkey_event(sender: PublicKey, identity: PublicKey) -> UnwrappedMessage {
        UnwrappedMessage {
            message: Message::Order(MessageKind::new(
                None,
                Some(1),
                None,
                Action::TradePubkey,
                None,
            )),
            signature: None,
            sender,
            identity,
            created_at: Timestamp::now(),
        }
    }

    fn trade_pubkey_msg(order_id: Uuid) -> Message {
        Message::new_order(Some(order_id), Some(1), None, Action::TradePubkey, None)
    }

    fn base_order(status: Status) -> Order {
        Order {
            id: Uuid::new_v4(),
            status: status.to_string(),
            kind: mostro_core::order::Kind::Sell.to_string(),
            fiat_code: "USD".to_string(),
            creator_pubkey: Keys::generate().public_key().to_string(),
            amount: 10_000,
            fee: 10,
            ..Default::default()
        }
    }

    async fn order_by_id(pool: &SqlitePool, id: Uuid) -> Order {
        Order::by_id(pool, id).await.unwrap().unwrap()
    }

    #[tokio::test]
    async fn trade_pubkey_action_rejects_non_pre_trade_status() {
        let ctx = setup_ctx().await;
        let identity = Keys::generate().public_key();

        let mut order = base_order(Status::Active);
        order.master_buyer_pubkey = Some(identity.to_string());
        let order = order.create(ctx.pool()).await.unwrap();

        let event = trade_pubkey_event(Keys::generate().public_key(), identity);
        let result = trade_pubkey_action(&ctx, trade_pubkey_msg(order.id), &event).await;

        assert!(matches!(
            result,
            Err(MostroCantDo(CantDoReason::InvalidOrderStatus))
        ));
    }

    #[tokio::test]
    async fn trade_pubkey_action_rotates_buyer_trade_key_for_master_buyer() {
        let ctx = setup_ctx().await;
        let identity = Keys::generate().public_key();
        let new_trade_key = Keys::generate().public_key();

        let mut order = base_order(Status::Pending);
        order.master_buyer_pubkey = Some(identity.to_string());
        order.buyer_pubkey = Some(Keys::generate().public_key().to_string());
        let order = order.create(ctx.pool()).await.unwrap();

        let event = trade_pubkey_event(new_trade_key, identity);
        let result = trade_pubkey_action(&ctx, trade_pubkey_msg(order.id), &event).await;

        assert!(result.is_ok(), "buyer rotation must succeed: {result:?}");
        let after = order_by_id(ctx.pool(), order.id).await;
        assert_eq!(after.buyer_pubkey, Some(new_trade_key.to_string()));
        assert_eq!(after.creator_pubkey, new_trade_key.to_string());
        // The confirmation goes to the process-global queue; filter by
        // this test's unique trade key to stay isolated.
        let confirmations = crate::config::MESSAGE_QUEUES
            .queue_order_msg
            .read()
            .await
            .iter()
            .filter(|(m, pk)| {
                *pk == new_trade_key && m.get_inner_message_kind().action == Action::TradePubkey
            })
            .count();
        assert_eq!(confirmations, 1);
    }

    #[tokio::test]
    async fn trade_pubkey_action_rotates_seller_trade_key_for_master_seller() {
        let ctx = setup_ctx().await;
        let identity = Keys::generate().public_key();
        let new_trade_key = Keys::generate().public_key();

        // `WaitingTakerBond` is the other accepted pre-trade entry point.
        let mut order = base_order(Status::WaitingTakerBond);
        order.master_seller_pubkey = Some(identity.to_string());
        order.seller_pubkey = Some(Keys::generate().public_key().to_string());
        let order = order.create(ctx.pool()).await.unwrap();

        let event = trade_pubkey_event(new_trade_key, identity);
        let result = trade_pubkey_action(&ctx, trade_pubkey_msg(order.id), &event).await;

        assert!(result.is_ok(), "seller rotation must succeed: {result:?}");
        let after = order_by_id(ctx.pool(), order.id).await;
        assert_eq!(after.seller_pubkey, Some(new_trade_key.to_string()));
        assert_eq!(after.creator_pubkey, new_trade_key.to_string());
    }

    #[tokio::test]
    async fn trade_pubkey_action_rejects_identity_that_does_not_own_the_order() {
        let ctx = setup_ctx().await;

        let mut order = base_order(Status::Pending);
        order.master_buyer_pubkey = Some(Keys::generate().public_key().to_string());
        let order = order.create(ctx.pool()).await.unwrap();

        // A different identity than the stored master buyer key.
        let event =
            trade_pubkey_event(Keys::generate().public_key(), Keys::generate().public_key());
        let result = trade_pubkey_action(&ctx, trade_pubkey_msg(order.id), &event).await;

        assert!(matches!(
            result,
            Err(MostroInternalErr(ServiceError::InvalidPubkey))
        ));
        // And the order must be left untouched.
        let after = order_by_id(ctx.pool(), order.id).await;
        assert_eq!(after.creator_pubkey, order.creator_pubkey);
    }

    #[tokio::test]
    async fn trade_pubkey_action_errors_when_no_master_key_is_stored() {
        let ctx = setup_ctx().await;

        // Neither master key present: the master-seller getter errors.
        let order = base_order(Status::Pending)
            .create(ctx.pool())
            .await
            .unwrap();

        let event =
            trade_pubkey_event(Keys::generate().public_key(), Keys::generate().public_key());
        let result = trade_pubkey_action(&ctx, trade_pubkey_msg(order.id), &event).await;

        assert!(matches!(
            result,
            Err(MostroInternalErr(ServiceError::InvalidPubkey))
        ));
    }
}
