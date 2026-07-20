use crate::app::bond;
use crate::app::bond::TakerContext;
use crate::app::context::AppContext;
use crate::config::settings::Settings;
use crate::util::{
    enqueue_order_msg, get_dev_fee, get_fiat_amount_requested, get_market_amount_and_fee,
    get_order, show_cashu_escrow_request, show_hold_invoice,
};

use crate::db::{seller_has_pending_order, update_user_trade_index};
use mostro_core::prelude::*;
use nostr_sdk::prelude::*;

pub async fn take_buy_action(
    ctx: &AppContext,
    msg: Message,
    event: &UnwrappedMessage,
    my_keys: &Keys,
) -> Result<(), MostroError> {
    let pool = ctx.pool();
    // Extract order ID from the message, returning an error if not found
    // Safe unwrap as we verified the message
    let mut order = get_order(&msg, pool).await?;

    // Get the request ID from the message
    let request_id = msg.get_inner_message_kind().request_id;

    // Check if the buyer has a pending order
    if seller_has_pending_order(pool, event.identity.to_string()).await? {
        return Err(MostroCantDo(CantDoReason::PendingOrderExists));
    }

    // Check if the order is a buy order and if its status is active
    if let Err(cause) = order.is_buy_order() {
        return Err(MostroCantDo(cause));
    };
    // Accept takes against orders in either `Pending` (no taker yet) or
    // `WaitingTakerBond` (Phase 1.5: a prior concurrent taker is
    // mid-bond). Both are pre-trade from the take-validation
    // perspective; the locked-bond gate inside the bond block below
    // catches the genuine post-trade case.
    if order.check_status(Status::Pending).is_err()
        && order.check_status(Status::WaitingTakerBond).is_err()
    {
        return Err(MostroCantDo(CantDoReason::InvalidOrderStatus));
    }

    // Validate that the order was sent from the correct maker
    order
        .not_sent_from_maker(event.sender)
        .map_err(MostroCantDo)?;

    // Anti-abuse bond (Phase 1, concurrent-bonds model). The take
    // handler doesn't release prior bonds at retake-time anymore —
    // multiple `Requested` taker bonds coexist on the order and the
    // first to reach `Locked` wins. We still need three guards here:
    //   1. A `Locked` *taker* bond already on the order means the
    //      trade is committed; reject with `PendingOrderExists`.
    //   2. The sender's own pubkey already has a `Requested` bond on
    //      this order → idempotent retry: re-send the same
    //      `PayInvoice` message and return.
    //   3. Otherwise fall through and create a fresh bond row.
    // We do *not* mutate the order's taker fields under the bond
    // path; that context lives on the bond row's `taker_*` columns
    // until the winning bond locks and
    // `on_bond_invoice_accepted` promotes it onto the order.
    //
    // Phase 5: with `apply_to = both` the maker's own bond is already
    // `Locked` on every published order — that is the normal state, not
    // a committed trade. The committed-trade gate must therefore only
    // count `Locked` *taker* bonds; otherwise a locked maker bond would
    // wrongly block every taker with `PendingOrderExists`.
    let bond_required = bond::taker_bond_required();
    if bond_required {
        let active = crate::app::bond::db::find_active_bonds_for_order(pool, order.id).await?;
        if bond::trade_committed_by_locked_taker_bond(&active) {
            return Err(MostroCantDo(CantDoReason::PendingOrderExists));
        }
        let sender_str = event.sender.to_string();
        if let Some(existing) = active.iter().find(|b| b.pubkey == sender_str) {
            if let Some(bolt11) = existing.payment_request.clone() {
                let order_kind = order.get_order_kind().map_err(MostroInternalErr)?;
                let bond_small = SmallOrder::new(
                    Some(order.id),
                    Some(order_kind),
                    Some(Status::Pending),
                    existing.amount_sats,
                    order.fiat_code.clone(),
                    order.min_amount,
                    order.max_amount,
                    existing.taker_fiat_amount.unwrap_or(order.fiat_amount),
                    order.payment_method.clone(),
                    order.premium,
                    None,
                    None,
                    None,
                    None,
                    None,
                );
                enqueue_order_msg(
                    request_id,
                    Some(order.id),
                    Action::PayBondInvoice,
                    Some(Payload::PaymentRequest(Some(bond_small), bolt11, None)),
                    event.sender,
                    existing.taker_trade_index,
                )
                .await;
            }
            return Ok(());
        }
    }

    // Get the fiat amount requested by the user for range orders
    if let Some(am) = get_fiat_amount_requested(&order, &msg) {
        order.fiat_amount = am;
    } else {
        return Err(MostroCantDo(CantDoReason::OutOfRangeSatsAmount));
    }

    // If the order amount is zero, calculate the market price in sats
    if order.has_no_amount() {
        match get_market_amount_and_fee(order.fiat_amount, &order.fiat_code, order.premium) {
            Ok(amount_fees) => {
                order.amount = amount_fees.0;
                order.fee = amount_fees.1;
                // Calculate dev_fee now that we know the fee amount
                let total_mostro_fee = order.fee * 2;
                order.dev_fee = get_dev_fee(total_mostro_fee);
            }
            // No fresh rate within the staleness window — refuse cleanly so
            // the taker can retry rather than pricing on stale data.
            Err(MostroInternalErr(ServiceError::PriceTooStale)) => {
                return Err(MostroCantDo(CantDoReason::PriceTooStale))
            }
            Err(_) => return Err(MostroInternalErr(ServiceError::WrongAmountError)),
        };
    } else {
        // Calculate dev_fee for fixed price orders
        // The fee is already calculated at order creation, we only calculate dev_fee here
        let total_mostro_fee = order.fee * 2;
        order.dev_fee = get_dev_fee(total_mostro_fee);
    }

    // Get seller and buyer public keys
    let seller_pubkey = event.sender;
    let buyer_pubkey = order.get_buyer_pubkey().map_err(MostroInternalErr)?;

    // Resolve the trade index for this take (or 0 when identity == sender).
    let trade_index = match msg.get_inner_message_kind().trade_index {
        Some(trade_index) => trade_index,
        None => {
            if event.identity == event.sender {
                0
            } else {
                return Err(MostroInternalErr(ServiceError::InvalidPayload));
            }
        }
    };

    // Update trade index only after all checks are done. We bump
    // per-take (regardless of who wins the bond race) so the user's
    // monotonic trade-index counter stays consistent across attempts.
    update_user_trade_index(pool, event.identity.to_string(), trade_index)
        .await
        .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

    // Concurrent-bonds path: stash this take's context on the bond
    // row, leave the order untouched. The winning bond's
    // `on_bond_invoice_accepted` callback will copy the `taker_*`
    // snapshot onto the order at lock-time and drive the trade flow.
    if bond_required {
        let taker_ctx = TakerContext {
            identity: event.identity.to_string(),
            trade_index,
            buyer_invoice: None,
            fiat_amount: order.fiat_amount,
            amount: order.amount,
            fee: order.fee,
            dev_fee: order.dev_fee,
        };
        bond::request_taker_bond(pool, &order, seller_pubkey, request_id, taker_ctx).await?;
        return Ok(());
    }

    // Non-bond path: legacy take. Persist the taker fields on the
    // order before driving the trade hold invoice.
    order.master_seller_pubkey = Some(event.identity.to_string());
    order.trade_index_seller = Some(trade_index);
    order.set_timestamp_now();

    // Cashu escrow mode (Track A TA-2): the seller (taker) locks a 2-of-3 token
    // instead of paying a hold invoice. Emit the escrow request and leave the
    // order in WaitingPayment, where the CAS in `add_cashu_escrow_action`
    // expects it.
    if Settings::is_cashu_enabled() {
        show_cashu_escrow_request(
            pool,
            my_keys,
            &buyer_pubkey,
            &seller_pubkey,
            order,
            request_id,
        )
        .await?;
        return Ok(());
    }

    // Show hold invoice and return success or error
    if let Err(cause) = show_hold_invoice(
        my_keys,
        None,
        &buyer_pubkey,
        &seller_pubkey,
        order,
        request_id,
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
    use mostro_core::db::Crud;
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
        // With `anti_abuse_bond = None`, `taker_bond_required()` is false,
        // so takes flow through the legacy `show_hold_invoice` path.
        let _ = crate::config::MOSTRO_CONFIG.set(test_settings());
        TestContextBuilder::new()
            .with_pool(pool)
            .with_settings(test_settings())
            .build()
    }

    /// For a buy order, the taker is the seller. `event.sender` is that
    /// taker trade key; `identity` matches `sender` so a `None`
    /// trade_index resolves to 0.
    fn taker_event(taker: PublicKey) -> UnwrappedMessage {
        UnwrappedMessage {
            message: Message::new_order(
                Some(uuid::Uuid::new_v4()),
                Some(1),
                None,
                Action::TakeBuy,
                None,
            ),
            signature: None,
            sender: taker,
            identity: taker,
            created_at: Timestamp::now(),
        }
    }

    /// A pending buy order: the maker is the buyer (creator ==
    /// buyer_pubkey), no taker yet.
    fn pending_buy_order(maker_buyer: PublicKey) -> Order {
        Order {
            id: uuid::Uuid::new_v4(),
            status: Status::Pending.to_string(),
            kind: mostro_core::order::Kind::Buy.to_string(),
            fiat_code: "USD".to_string(),
            fiat_amount: 100,
            creator_pubkey: maker_buyer.to_string(),
            buyer_pubkey: Some(maker_buyer.to_string()),
            master_buyer_pubkey: Some(maker_buyer.to_string()),
            amount: 21_000,
            fee: 210,
            ..Default::default()
        }
    }

    fn take_buy_msg(order_id: uuid::Uuid, trade_index: Option<i64>) -> Message {
        Message::new_order(Some(order_id), Some(1), trade_index, Action::TakeBuy, None)
    }

    #[tokio::test]
    async fn take_buy_action_fails_when_order_missing() {
        let pool = setup_pool().await;
        let ctx = build_ctx(pool);
        let taker = Keys::generate().public_key();

        let result = take_buy_action(
            &ctx,
            take_buy_msg(uuid::Uuid::new_v4(), Some(1)),
            &taker_event(taker),
            &Keys::generate(),
        )
        .await;

        assert!(matches!(result, Err(MostroCantDo(CantDoReason::NotFound))));
    }

    #[tokio::test]
    async fn take_buy_action_rejects_taker_with_pending_order() {
        let pool = setup_pool().await;
        let ctx = build_ctx(pool.clone());
        let maker = Keys::generate().public_key();
        let taker = Keys::generate().public_key();

        let order = pending_buy_order(maker).create(ctx.pool()).await.unwrap();

        // The taker already has a waiting-payment order as seller.
        let mut pending = pending_buy_order(Keys::generate().public_key());
        pending.status = Status::WaitingPayment.to_string();
        pending.master_seller_pubkey = Some(taker.to_string());
        pending.create(ctx.pool()).await.unwrap();

        let result = take_buy_action(
            &ctx,
            take_buy_msg(order.id, Some(1)),
            &taker_event(taker),
            &Keys::generate(),
        )
        .await;

        assert!(matches!(
            result,
            Err(MostroCantDo(CantDoReason::PendingOrderExists))
        ));
    }

    #[tokio::test]
    async fn take_buy_action_rejects_non_buy_order() {
        let pool = setup_pool().await;
        let ctx = build_ctx(pool.clone());
        let maker = Keys::generate().public_key();
        let taker = Keys::generate().public_key();

        let mut order = pending_buy_order(maker);
        order.kind = mostro_core::order::Kind::Sell.to_string();
        let order = order.create(ctx.pool()).await.unwrap();

        let result = take_buy_action(
            &ctx,
            take_buy_msg(order.id, Some(1)),
            &taker_event(taker),
            &Keys::generate(),
        )
        .await;

        assert!(matches!(
            result,
            Err(MostroCantDo(CantDoReason::InvalidOrderKind))
        ));
    }

    #[tokio::test]
    async fn take_buy_action_rejects_non_pending_status() {
        let pool = setup_pool().await;
        let ctx = build_ctx(pool.clone());
        let maker = Keys::generate().public_key();
        let taker = Keys::generate().public_key();

        let mut order = pending_buy_order(maker);
        order.status = Status::Active.to_string();
        let order = order.create(ctx.pool()).await.unwrap();

        let result = take_buy_action(
            &ctx,
            take_buy_msg(order.id, Some(1)),
            &taker_event(taker),
            &Keys::generate(),
        )
        .await;

        assert!(matches!(
            result,
            Err(MostroCantDo(CantDoReason::InvalidOrderStatus))
        ));
    }

    /// The maker cannot take their own order: `not_sent_from_maker` fails
    /// when `event.sender == creator_pubkey`.
    #[tokio::test]
    async fn take_buy_action_rejects_maker_taking_own_order() {
        let pool = setup_pool().await;
        let ctx = build_ctx(pool.clone());
        let maker = Keys::generate().public_key();

        let order = pending_buy_order(maker).create(ctx.pool()).await.unwrap();

        let result = take_buy_action(
            &ctx,
            take_buy_msg(order.id, Some(1)),
            &taker_event(maker),
            &Keys::generate(),
        )
        .await;

        assert!(matches!(
            result,
            Err(MostroCantDo(CantDoReason::InvalidPubkey))
        ));
    }

    /// A range order for which the taker requested no amount is rejected
    /// with `OutOfRangeSatsAmount`.
    #[tokio::test]
    async fn take_buy_action_rejects_range_order_without_amount() {
        let pool = setup_pool().await;
        let ctx = build_ctx(pool.clone());
        let maker = Keys::generate().public_key();
        let taker = Keys::generate().public_key();

        let mut order = pending_buy_order(maker);
        order.min_amount = Some(10);
        order.max_amount = Some(1000);
        let order = order.create(ctx.pool()).await.unwrap();

        // Message carries no amount payload for the range order.
        let result = take_buy_action(
            &ctx,
            take_buy_msg(order.id, Some(1)),
            &taker_event(taker),
            &Keys::generate(),
        )
        .await;

        assert!(matches!(
            result,
            Err(MostroCantDo(CantDoReason::OutOfRangeSatsAmount))
        ));
    }

    /// When `identity != sender` and the message omits a trade index, the
    /// take is rejected with `InvalidPayload`.
    #[tokio::test]
    async fn take_buy_action_requires_trade_index_when_identity_differs() {
        let pool = setup_pool().await;
        let ctx = build_ctx(pool.clone());
        let maker = Keys::generate().public_key();
        let taker = Keys::generate().public_key();

        let order = pending_buy_order(maker).create(ctx.pool()).await.unwrap();

        let mut event = taker_event(taker);
        event.identity = Keys::generate().public_key(); // differs from sender
        let result = take_buy_action(
            &ctx,
            take_buy_msg(order.id, None),
            &event,
            &Keys::generate(),
        )
        .await;

        assert!(matches!(
            result,
            Err(MostroInternalErr(ServiceError::InvalidPayload))
        ));
    }

    /// Full non-bond happy path: all validation passes, the trade index is
    /// bumped and dev fee computed, and the handler reaches
    /// `show_hold_invoice`, which fails offline at `LndConnector::new()`.
    /// The hold-invoice creation tail is covered by integration tests.
    #[tokio::test]
    async fn take_buy_action_reaches_hold_invoice_seam_on_happy_path() {
        let pool = setup_pool().await;
        let ctx = build_ctx(pool.clone());
        let maker = Keys::generate().public_key();
        let taker = Keys::generate().public_key();

        let order = pending_buy_order(maker).create(ctx.pool()).await.unwrap();

        let result = take_buy_action(
            &ctx,
            take_buy_msg(order.id, Some(1)),
            &taker_event(taker),
            &Keys::generate(),
        )
        .await;

        assert!(matches!(
            result,
            Err(MostroInternalErr(ServiceError::HoldInvoiceError(_)))
        ));
    }
}
