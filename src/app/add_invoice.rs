use crate::app::context::AppContext;
use crate::util::{
    enqueue_order_msg, get_order, notify_taker_reputation, show_hold_invoice, update_order_event,
    validate_invoice,
};
use mostro_core::db::Crud;
use mostro_core::prelude::*;
use nostr_sdk::prelude::*;
use sqlx::{Pool, Sqlite};

pub async fn pay_new_invoice(
    order: &mut Order,
    pool: &Pool<Sqlite>,
    msg: &Message,
) -> Result<(), MostroError> {
    order.payment_attempts = 0;
    order
        .clone()
        .update(pool)
        .await
        .map_err(|cause| MostroInternalErr(ServiceError::DbAccessError(cause.to_string())))?;
    enqueue_order_msg(
        msg.get_inner_message_kind().request_id,
        Some(order.id),
        Action::InvoiceUpdated,
        None,
        order.get_buyer_pubkey().map_err(MostroInternalErr)?,
        None,
    )
    .await;
    Ok(())
}

pub async fn add_invoice_action(
    ctx: &AppContext,
    msg: Message,
    event: &UnwrappedMessage,
    my_keys: &Keys,
) -> Result<(), MostroError> {
    let pool = ctx.pool();
    // Get order
    let mut order = get_order(&msg, pool).await?;
    // Check order status
    let ord_status = order.get_order_status().map_err(MostroInternalErr)?;
    // Check order kind
    order.get_order_kind().map_err(MostroInternalErr)?;
    // Get buyer pubkey
    let buyer_pubkey = order.get_buyer_pubkey().map_err(MostroInternalErr)?;
    // Only the buyer can add an invoice
    if buyer_pubkey != event.sender {
        return Err(MostroCantDo(CantDoReason::InvalidPeer));
    }
    // We save the invoice on db
    order.buyer_invoice = validate_invoice(&msg, &order).await?;
    // Buyer can add invoice orders with WaitingBuyerInvoice status
    match ord_status {
        Status::SettledHoldInvoice => {
            pay_new_invoice(&mut order, pool, &msg).await?;
            return Ok(());
        }
        Status::WaitingBuyerInvoice => {}
        _ => {
            return Err(MostroCantDo(CantDoReason::NotAllowedByStatus));
        }
    }

    // Notify taker reputation
    tracing::info!("Notifying taker reputation to maker");
    notify_taker_reputation(pool, &order).await?;

    // Get seller pubkey
    let seller_pubkey = order.get_seller_pubkey().map_err(MostroInternalErr)?;
    // Check if the order has a preimage
    if order.preimage.is_some() {
        // We publish a new replaceable kind nostr event with the status updated
        // and update on local database the status and new event id
        let active_order = match update_order_event(my_keys, Status::Active, &order).await {
            Ok(updated_order) => {
                // Update in database
                updated_order.clone().update(pool).await.map_err(|cause| {
                    MostroInternalErr(ServiceError::DbAccessError(cause.to_string()))
                })?;
                updated_order
            }
            Err(e) => return Err(e),
        };

        // We send a confirmation message to seller
        let mut seller_order = SmallOrder::from(active_order.clone());
        seller_order.amount = active_order.amount.saturating_add(active_order.fee);
        // Clear buyer_invoice to avoid leaking buyer's payment info to seller
        seller_order.buyer_invoice = None;
        enqueue_order_msg(
            None,
            Some(active_order.id),
            Action::BuyerTookOrder,
            Some(Payload::Order(seller_order)),
            seller_pubkey,
            None,
        )
        .await;
        // We send a message to buyer saying seller paid
        let mut buyer_order = SmallOrder::from(active_order.clone());
        buyer_order.amount = active_order.amount.saturating_sub(active_order.fee);
        enqueue_order_msg(
            msg.get_inner_message_kind().request_id,
            Some(active_order.id),
            Action::HoldInvoicePaymentAccepted,
            Some(Payload::Order(buyer_order)),
            buyer_pubkey,
            None,
        )
        .await;
    } else if let Err(cause) = show_hold_invoice(
        my_keys,
        None,
        &buyer_pubkey,
        &seller_pubkey,
        order,
        msg.get_inner_message_kind().request_id,
    )
    .await
    {
        return Err(MostroInternalErr(ServiceError::HoldInvoiceError(
            cause.to_string(),
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::context::test_utils::{test_settings, TestContextBuilder};
    use sqlx::SqlitePool;
    use std::sync::Arc;

    async fn setup_pool() -> Arc<SqlitePool> {
        let pool = Arc::new(SqlitePool::connect("sqlite::memory:").await.unwrap());
        sqlx::migrate!("./migrations")
            .run(pool.as_ref())
            .await
            .unwrap();
        pool
    }

    fn build_ctx(pool: Arc<SqlitePool>) -> AppContext {
        // update_order_event reads the global config (expiration/nostr);
        // seeding it is idempotent and safe under concurrent tests.
        let _ = crate::config::MOSTRO_CONFIG.set(test_settings());
        TestContextBuilder::new()
            .with_pool(pool)
            .with_settings(test_settings())
            .build()
    }

    /// `event.sender` is the buyer trade key (only the buyer may add an
    /// invoice); `identity` is an unrelated key so tests reflect the
    /// dual-key flow.
    fn buyer_event(buyer: PublicKey) -> UnwrappedMessage {
        UnwrappedMessage {
            message: Message::new_order(
                Some(uuid::Uuid::new_v4()),
                Some(1),
                None,
                Action::AddInvoice,
                None,
            ),
            signature: None,
            sender: buyer,
            identity: Keys::generate().public_key(),
            created_at: Timestamp::now(),
        }
    }

    /// A sell order where the buyer is the taker adding a payout invoice.
    fn waiting_invoice_sell_order(seller: PublicKey, buyer: PublicKey) -> Order {
        Order {
            id: uuid::Uuid::new_v4(),
            status: Status::WaitingBuyerInvoice.to_string(),
            kind: mostro_core::order::Kind::Sell.to_string(),
            fiat_code: "USD".to_string(),
            creator_pubkey: seller.to_string(),
            seller_pubkey: Some(seller.to_string()),
            master_seller_pubkey: Some(seller.to_string()),
            buyer_pubkey: Some(buyer.to_string()),
            master_buyer_pubkey: Some(buyer.to_string()),
            amount: 21_000,
            fee: 210,
            ..Default::default()
        }
    }

    fn add_invoice_msg(order_id: uuid::Uuid) -> Message {
        Message::new_order(Some(order_id), Some(1), None, Action::AddInvoice, None)
    }

    async fn queued_actions_for(destination: PublicKey) -> Vec<Action> {
        crate::config::MESSAGE_QUEUES
            .queue_order_msg
            .read()
            .await
            .iter()
            .filter(|(_, pk)| *pk == destination)
            .map(|(m, _)| m.get_inner_message_kind().action.clone())
            .collect()
    }

    #[tokio::test]
    async fn add_invoice_action_fails_when_order_missing() {
        let pool = setup_pool().await;
        let ctx = build_ctx(pool);
        let buyer = Keys::generate();
        let event = buyer_event(buyer.public_key());

        let result = add_invoice_action(
            &ctx,
            add_invoice_msg(uuid::Uuid::new_v4()),
            &event,
            &Keys::generate(),
        )
        .await;

        assert!(matches!(result, Err(MostroCantDo(CantDoReason::NotFound))));
    }

    #[tokio::test]
    async fn add_invoice_action_rejects_non_buyer_sender() {
        let pool = setup_pool().await;
        let ctx = build_ctx(pool.clone());
        let seller = Keys::generate().public_key();
        let buyer = Keys::generate().public_key();

        let order = waiting_invoice_sell_order(seller, buyer)
            .create(ctx.pool())
            .await
            .unwrap();

        // The event is authored by an intruder, not the buyer.
        let event = buyer_event(Keys::generate().public_key());
        let result =
            add_invoice_action(&ctx, add_invoice_msg(order.id), &event, &Keys::generate()).await;

        assert!(matches!(
            result,
            Err(MostroCantDo(CantDoReason::InvalidPeer))
        ));
    }

    #[tokio::test]
    async fn add_invoice_action_rejects_disallowed_status() {
        let pool = setup_pool().await;
        let ctx = build_ctx(pool.clone());
        let seller = Keys::generate().public_key();
        let buyer = Keys::generate().public_key();

        let mut order = waiting_invoice_sell_order(seller, buyer);
        order.status = Status::Active.to_string();
        let order = order.create(ctx.pool()).await.unwrap();

        let event = buyer_event(buyer);
        let result =
            add_invoice_action(&ctx, add_invoice_msg(order.id), &event, &Keys::generate()).await;

        assert!(matches!(
            result,
            Err(MostroCantDo(CantDoReason::NotAllowedByStatus))
        ));
    }

    /// A `SettledHoldInvoice` order routes through `pay_new_invoice`: the
    /// payment-attempts counter is reset and the buyer is told the invoice
    /// was updated. No LND is involved so the handler returns `Ok`.
    #[tokio::test]
    async fn add_invoice_action_settled_hold_invoice_resets_and_notifies_buyer() {
        let pool = setup_pool().await;
        let ctx = build_ctx(pool.clone());
        let seller = Keys::generate().public_key();
        let buyer = Keys::generate().public_key();

        let mut order = waiting_invoice_sell_order(seller, buyer);
        order.status = Status::SettledHoldInvoice.to_string();
        order.payment_attempts = 3;
        let order = order.create(ctx.pool()).await.unwrap();

        let event = buyer_event(buyer);
        let result =
            add_invoice_action(&ctx, add_invoice_msg(order.id), &event, &Keys::generate()).await;

        assert!(
            result.is_ok(),
            "settled-hold-invoice path must succeed: {result:?}"
        );
        let stored = Order::by_id(ctx.pool(), order.id).await.unwrap().unwrap();
        assert_eq!(stored.payment_attempts, 0, "attempts must be reset");
        assert!(queued_actions_for(buyer)
            .await
            .contains(&Action::InvoiceUpdated));
    }

    /// Happy path for a `WaitingBuyerInvoice` order that already has a
    /// preimage (seller paid the hold invoice first): the order is
    /// promoted to `Active`, the seller is told the buyer took the order,
    /// and the buyer is told the hold invoice payment was accepted. The
    /// buyer's invoice is stripped from the seller-facing message.
    #[tokio::test]
    async fn add_invoice_action_with_preimage_activates_and_notifies_both() {
        let pool = setup_pool().await;
        let ctx = build_ctx(pool.clone());
        let seller = Keys::generate().public_key();
        let buyer = Keys::generate().public_key();

        let mut order = waiting_invoice_sell_order(seller, buyer);
        order.preimage = Some("deadbeef".to_string());
        let order = order.create(ctx.pool()).await.unwrap();

        let event = buyer_event(buyer);
        let result =
            add_invoice_action(&ctx, add_invoice_msg(order.id), &event, &Keys::generate()).await;

        assert!(result.is_ok(), "preimage path must succeed: {result:?}");
        let stored = Order::by_id(ctx.pool(), order.id).await.unwrap().unwrap();
        assert_eq!(stored.status, Status::Active.to_string());
        assert!(queued_actions_for(seller)
            .await
            .contains(&Action::BuyerTookOrder));
        assert!(queued_actions_for(buyer)
            .await
            .contains(&Action::HoldInvoicePaymentAccepted));
    }

    /// Without a preimage the handler falls through to `show_hold_invoice`,
    /// which calls `LndConnector::new()` and fails offline. Everything up to
    /// that LND seam is covered; the hold-invoice creation itself is
    /// exercised by integration tests against a live LND.
    #[tokio::test]
    async fn add_invoice_action_without_preimage_hits_hold_invoice_seam() {
        let pool = setup_pool().await;
        let ctx = build_ctx(pool.clone());
        let seller = Keys::generate().public_key();
        let buyer = Keys::generate().public_key();

        let order = waiting_invoice_sell_order(seller, buyer)
            .create(ctx.pool())
            .await
            .unwrap();

        let event = buyer_event(buyer);
        let result =
            add_invoice_action(&ctx, add_invoice_msg(order.id), &event, &Keys::generate()).await;

        assert!(matches!(
            result,
            Err(MostroInternalErr(ServiceError::HoldInvoiceError(_)))
        ));
    }
}
