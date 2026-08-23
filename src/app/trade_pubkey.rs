use crate::app::context::AppContext;
use crate::db::cas_rotate_maker_trade_pubkey;
use crate::util::{enqueue_order_msg, get_order};

use mostro_core::order::Kind as OrderKind;
use mostro_core::prelude::*;

/// Rotate the maker's per-trade pubkey for a pre-trade order.
///
/// Only the order maker may rotate: maker ownership is determined by the
/// order kind — the seller is the maker of a [`OrderKind::Sell`] order and
/// the buyer is the maker of a [`OrderKind::Buy`] order. The caller's
/// `event.identity` must match the maker-side master key; on success the
/// maker-side trade pubkey and `creator_pubkey` (which tracks the maker's
/// current trade key) are set to `event.sender`. Anything else is rejected
/// with [`ServiceError::InvalidPubkey`] and the order is left untouched.
///
/// The rotation is persisted with a status-guarded compare-and-swap and the
/// confirmation is sent only once that write lands.
pub async fn trade_pubkey_action(
    ctx: &AppContext,
    msg: Message,
    event: &UnwrappedMessage,
) -> Result<(), MostroError> {
    let pool = ctx.pool();
    // Get request id
    let request_id = msg.get_inner_message_kind().request_id;
    // Get order
    let order = get_order(&msg, pool).await?;

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

    // Only the maker may rotate the trade key, and the maker side is
    // fixed by the order kind: the creator of a sell order is the seller,
    // of a buy order the buyer. Match the caller's identity against the
    // maker's master key alone — never the taker's. The bond flow can
    // leave BOTH master keys on a pre-trade order
    // (`promote_taker_context_to_order`); keying the branch on
    // `master_buyer_pubkey.is_some()` would let a sell-order taker match
    // the buyer branch, and an unconditional `creator_pubkey` write would
    // then hand them maker attribution (`sent_from_maker` compares
    // `creator_pubkey`).
    let kind = order.get_order_kind().map_err(MostroInternalErr)?;
    let maker_master_key = match kind {
        OrderKind::Sell => order
            .get_master_seller_pubkey()
            .map_err(MostroInternalErr)?,
        OrderKind::Buy => order.get_master_buyer_pubkey().map_err(MostroInternalErr)?,
    };
    if maker_master_key != event.identity {
        return Err(MostroInternalErr(ServiceError::InvalidPubkey));
    }
    // Persist through the pre-trade compare-and-swap (#866): it moves the
    // maker-side trade pubkey and `creator_pubkey` — the maker is the order
    // creator, and `creator_pubkey` must never move for anyone else — and
    // nothing else, only while the order is still pre-trade.
    //
    // A full-row `Crud::update` here would write this handler's snapshot
    // over whatever committed since the read at the top. The window is
    // real: the post-bond resume keeps the order pre-trade across an LND
    // `create_hold_invoice` round trip, so a rotation racing it could
    // revert the committed take to `pending` with `hash`/`preimage`
    // NULLed, orphaning a hold invoice the seller had already paid.
    let rotated =
        cas_rotate_maker_trade_pubkey(pool, order.id, kind, &event.sender.to_string()).await?;
    if !rotated {
        // The order left the pre-trade window while we were validating.
        // Log it: this is the only trace the race leaves behind, and it is
        // what tells us in production that a rotation and a take collided.
        // `NotAllowedByStatus` matches the other CAS-miss sites
        // (`take_sell.rs`, `show_hold_invoice`, `show_cashu_escrow_request`);
        // the pre-check above keeps `InvalidOrderStatus`, the reason every
        // pre-check in the repo reports for a status it can see up front.
        tracing::info!(
            "trade pubkey rotation: order {} left the pre-trade window before the CAS — refusing the rotation",
            order.id
        );
        return Err(MostroCantDo(CantDoReason::NotAllowedByStatus));
    }

    // Confirm only once the rotation is durable (#811): announcing it
    // ahead of the write leaves the maker signing with a key the daemon
    // never stored.
    enqueue_order_msg(
        request_id,
        Some(order.id),
        Action::TradePubkey,
        None,
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
    use nostr_sdk::prelude::{Keys, PublicKey, Timestamp};
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

    fn base_order(kind: OrderKind, status: Status) -> Order {
        Order {
            id: Uuid::new_v4(),
            status: status.to_string(),
            kind: kind.to_string(),
            fiat_code: "USD".to_string(),
            creator_pubkey: Keys::generate().public_key().to_string(),
            amount: 10_000,
            fee: 10,
            ..Default::default()
        }
    }

    /// A coherent maker-owned order: the maker's trade key doubles as
    /// `creator_pubkey`, exactly as `prepare_new_order` persists it.
    fn maker_order(kind: OrderKind, status: Status, maker: &Keys) -> Order {
        let mut order = base_order(kind, status);
        order.creator_pubkey = maker.public_key().to_string();
        match kind {
            OrderKind::Sell => {
                order.seller_pubkey = Some(maker.public_key().to_string());
                order.master_seller_pubkey = Some(maker.public_key().to_string());
            }
            OrderKind::Buy => {
                order.buyer_pubkey = Some(maker.public_key().to_string());
                order.master_buyer_pubkey = Some(maker.public_key().to_string());
            }
        }
        order
    }

    async fn order_by_id(pool: &SqlitePool, id: Uuid) -> Order {
        Order::by_id(pool, id).await.unwrap().unwrap()
    }

    #[tokio::test]
    async fn trade_pubkey_action_rejects_non_pre_trade_status() {
        let ctx = setup_ctx().await;
        let maker = Keys::generate();

        let order = maker_order(OrderKind::Sell, Status::Active, &maker);
        let order = order.create(ctx.pool()).await.unwrap();

        let event = trade_pubkey_event(Keys::generate().public_key(), maker.public_key());
        let result = trade_pubkey_action(&ctx, trade_pubkey_msg(order.id), &event).await;

        assert!(matches!(
            result,
            Err(MostroCantDo(CantDoReason::InvalidOrderStatus))
        ));
    }

    #[tokio::test]
    async fn trade_pubkey_action_rotates_buyer_trade_key_for_buy_maker() {
        let ctx = setup_ctx().await;
        let maker = Keys::generate();
        let new_trade_key = Keys::generate().public_key();

        // On a buy order the maker is the buyer.
        let order = maker_order(OrderKind::Buy, Status::Pending, &maker);
        let order = order.create(ctx.pool()).await.unwrap();

        let event = trade_pubkey_event(new_trade_key, maker.public_key());
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
    async fn trade_pubkey_action_rotates_seller_trade_key_for_sell_maker() {
        let ctx = setup_ctx().await;
        let maker = Keys::generate();
        let new_trade_key = Keys::generate().public_key();

        // `WaitingTakerBond` is the other accepted pre-trade entry point.
        let order = maker_order(OrderKind::Sell, Status::WaitingTakerBond, &maker);
        let order = order.create(ctx.pool()).await.unwrap();

        let event = trade_pubkey_event(new_trade_key, maker.public_key());
        let result = trade_pubkey_action(&ctx, trade_pubkey_msg(order.id), &event).await;

        assert!(result.is_ok(), "seller rotation must succeed: {result:?}");
        let after = order_by_id(ctx.pool(), order.id).await;
        assert_eq!(after.seller_pubkey, Some(new_trade_key.to_string()));
        assert_eq!(after.creator_pubkey, new_trade_key.to_string());
    }

    #[tokio::test]
    async fn trade_pubkey_action_rejects_identity_that_does_not_own_the_order() {
        let ctx = setup_ctx().await;
        let maker = Keys::generate();

        let order = maker_order(OrderKind::Sell, Status::Pending, &maker);
        let order = order.create(ctx.pool()).await.unwrap();

        // A different identity than the stored master seller key.
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
        let order = base_order(OrderKind::Sell, Status::Pending)
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

    /// Regression test for the order-attribution hijack: the bond flow can
    /// leave BOTH master keys on a `Pending`/`WaitingTakerBond` order
    /// (`promote_taker_context_to_order` + a failed `resume_take_after_bond`).
    /// A sell-order taker whose identity is `master_buyer_pubkey` must not
    /// rotate anything — above all not `creator_pubkey`, which
    /// `sent_from_maker` compares for maker-only checks.
    #[tokio::test]
    async fn trade_pubkey_action_rejects_sell_taker_in_both_master_keys_window() {
        let ctx = setup_ctx().await;
        let maker = Keys::generate();
        let taker = Keys::generate();
        let attacker_fresh = Keys::generate().public_key();

        let mut order = maker_order(OrderKind::Sell, Status::Pending, &maker);
        order.buyer_pubkey = Some(taker.public_key().to_string());
        order.master_buyer_pubkey = Some(taker.public_key().to_string());
        let order = order.create(ctx.pool()).await.unwrap();

        let event = trade_pubkey_event(attacker_fresh, taker.public_key());
        let result = trade_pubkey_action(&ctx, trade_pubkey_msg(order.id), &event).await;

        assert!(matches!(
            result,
            Err(MostroInternalErr(ServiceError::InvalidPubkey))
        ));
        let after = order_by_id(ctx.pool(), order.id).await;
        assert_eq!(after.creator_pubkey, maker.public_key().to_string());
        assert_eq!(after.seller_pubkey, Some(maker.public_key().to_string()));
        assert_eq!(after.buyer_pubkey, Some(taker.public_key().to_string()));
    }

    /// Same both-master-keys window, but from the maker's side: the genuine
    /// creator of a sell order must still be able to rotate — keying the
    /// branch on `master_buyer_pubkey.is_some()` used to lock the maker out.
    #[tokio::test]
    async fn trade_pubkey_action_rotates_for_sell_maker_in_both_master_keys_window() {
        let ctx = setup_ctx().await;
        let maker = Keys::generate();
        let taker = Keys::generate();
        let new_trade_key = Keys::generate().public_key();

        let mut order = maker_order(OrderKind::Sell, Status::WaitingTakerBond, &maker);
        order.buyer_pubkey = Some(taker.public_key().to_string());
        order.master_buyer_pubkey = Some(taker.public_key().to_string());
        let order = order.create(ctx.pool()).await.unwrap();

        let event = trade_pubkey_event(new_trade_key, maker.public_key());
        let result = trade_pubkey_action(&ctx, trade_pubkey_msg(order.id), &event).await;

        assert!(result.is_ok(), "maker rotation must succeed: {result:?}");
        let after = order_by_id(ctx.pool(), order.id).await;
        assert_eq!(after.seller_pubkey, Some(new_trade_key.to_string()));
        assert_eq!(after.creator_pubkey, new_trade_key.to_string());
        // The taker side is untouched.
        assert_eq!(after.buyer_pubkey, Some(taker.public_key().to_string()));
    }

    /// Symmetric case on a buy order: the taker is the seller side, and the
    /// seller master key must not pass the maker check either.
    #[tokio::test]
    async fn trade_pubkey_action_rejects_buy_taker_in_both_master_keys_window() {
        let ctx = setup_ctx().await;
        let maker = Keys::generate();
        let taker = Keys::generate();
        let attacker_fresh = Keys::generate().public_key();

        let mut order = maker_order(OrderKind::Buy, Status::Pending, &maker);
        order.seller_pubkey = Some(taker.public_key().to_string());
        order.master_seller_pubkey = Some(taker.public_key().to_string());
        let order = order.create(ctx.pool()).await.unwrap();

        let event = trade_pubkey_event(attacker_fresh, taker.public_key());
        let result = trade_pubkey_action(&ctx, trade_pubkey_msg(order.id), &event).await;

        assert!(matches!(
            result,
            Err(MostroInternalErr(ServiceError::InvalidPubkey))
        ));
        let after = order_by_id(ctx.pool(), order.id).await;
        assert_eq!(after.creator_pubkey, maker.public_key().to_string());
        assert_eq!(after.buyer_pubkey, Some(maker.public_key().to_string()));
        assert_eq!(after.seller_pubkey, Some(taker.public_key().to_string()));
    }

    /// #811: the confirmation must never precede the persist. A rotation
    /// the handler rejects announces nothing to the caller.
    ///
    /// This is the *pre-check* rejection: the status is already out of the
    /// pre-trade window when the handler reads the order, so it never
    /// reaches the CAS. The CAS-miss branch is covered by
    /// `trade_pubkey_action_does_not_confirm_a_cas_miss` below.
    #[tokio::test]
    async fn trade_pubkey_action_does_not_confirm_a_rejected_rotation() {
        let ctx = setup_ctx().await;
        let maker = Keys::generate();
        let sender = Keys::generate().public_key();

        // Past the pre-trade window: the status gate rejects.
        let order = maker_order(OrderKind::Sell, Status::WaitingPayment, &maker);
        let order = order.create(ctx.pool()).await.unwrap();

        let event = trade_pubkey_event(sender, maker.public_key());
        let result = trade_pubkey_action(&ctx, trade_pubkey_msg(order.id), &event).await;

        assert!(matches!(
            result,
            Err(MostroCantDo(CantDoReason::InvalidOrderStatus))
        ));
        // The queue is process-global; filter by this test's unique key.
        let confirmations = crate::config::MESSAGE_QUEUES
            .queue_order_msg
            .read()
            .await
            .iter()
            .filter(|(m, pk)| {
                *pk == sender && m.get_inner_message_kind().action == Action::TradePubkey
            })
            .count();
        assert_eq!(
            confirmations, 0,
            "a rejected rotation must not be confirmed"
        );
    }

    /// The other half of #811, and the only handler-level coverage of the
    /// `!rotated` branch: the order is still pre-trade when the handler
    /// reads it, so validation passes and the CAS runs — but a concurrent
    /// writer has moved the row out of the pre-trade window in between, so
    /// the guarded `UPDATE` matches nothing.
    ///
    /// That interleaving has no injection point between `get_order` and the
    /// CAS, so a `BEFORE UPDATE` trigger stands in for the racing writer:
    /// `RAISE(IGNORE)` skips the row, which is exactly what the handler sees
    /// from a status guard that no longer matches — `rows_affected() == 0`.
    /// The handler must reject and stay silent; the row must be untouched.
    #[tokio::test]
    async fn trade_pubkey_action_does_not_confirm_a_cas_miss() {
        let ctx = setup_ctx().await;
        let maker = Keys::generate();
        let sender = Keys::generate().public_key();

        let order = maker_order(OrderKind::Sell, Status::Pending, &maker);
        let order = order.create(ctx.pool()).await.unwrap();

        sqlx::query(
            "CREATE TRIGGER rotation_loses_the_cas BEFORE UPDATE ON orders \
             WHEN NEW.creator_pubkey <> OLD.creator_pubkey \
             BEGIN SELECT RAISE(IGNORE); END",
        )
        .execute(ctx.pool())
        .await
        .unwrap();

        let event = trade_pubkey_event(sender, maker.public_key());
        let result = trade_pubkey_action(&ctx, trade_pubkey_msg(order.id), &event).await;

        assert!(
            matches!(result, Err(MostroCantDo(CantDoReason::NotAllowedByStatus))),
            "a CAS miss must be reported as NotAllowedByStatus: {result:?}"
        );

        // Nothing moved, and nothing was announced.
        let after = order_by_id(ctx.pool(), order.id).await;
        assert_eq!(after.creator_pubkey, maker.public_key().to_string());
        assert_eq!(after.seller_pubkey, Some(maker.public_key().to_string()));
        // The queue is process-global; filter by this test's unique key.
        let confirmations = crate::config::MESSAGE_QUEUES
            .queue_order_msg
            .read()
            .await
            .iter()
            .filter(|(m, pk)| {
                *pk == sender && m.get_inner_message_kind().action == Action::TradePubkey
            })
            .count();
        assert_eq!(confirmations, 0, "a CAS miss must not be confirmed");
    }

    /// The rotation is a targeted write: a pre-trade order carrying a
    /// promoted taker context and trade escrow material keeps both. The
    /// full-row write this replaced would have NULLed them from the
    /// handler's own snapshot whenever that snapshot was stale.
    #[tokio::test]
    async fn trade_pubkey_action_leaves_taker_context_and_escrow_material_untouched() {
        let ctx = setup_ctx().await;
        let maker = Keys::generate();
        let taker = Keys::generate();
        let new_trade_key = Keys::generate().public_key();

        let mut order = maker_order(OrderKind::Sell, Status::WaitingTakerBond, &maker);
        order.buyer_pubkey = Some(taker.public_key().to_string());
        order.master_buyer_pubkey = Some(taker.public_key().to_string());
        order.hash = Some("aa".repeat(32));
        order.preimage = Some("bb".repeat(32));
        order.taken_at = 1_700_000_000;
        let order = order.create(ctx.pool()).await.unwrap();

        let event = trade_pubkey_event(new_trade_key, maker.public_key());
        let result = trade_pubkey_action(&ctx, trade_pubkey_msg(order.id), &event).await;
        assert!(result.is_ok(), "maker rotation must succeed: {result:?}");

        let after = order_by_id(ctx.pool(), order.id).await;
        assert_eq!(after.seller_pubkey, Some(new_trade_key.to_string()));
        assert_eq!(after.creator_pubkey, new_trade_key.to_string());
        assert_eq!(after.status, Status::WaitingTakerBond.to_string());
        assert_eq!(after.buyer_pubkey, Some(taker.public_key().to_string()));
        assert_eq!(
            after.master_buyer_pubkey,
            Some(taker.public_key().to_string())
        );
        assert_eq!(after.hash, Some("aa".repeat(32)));
        assert_eq!(after.preimage, Some("bb".repeat(32)));
        assert_eq!(after.taken_at, 1_700_000_000);
    }
}
