//! `payment-history`: aggregate history of the buyer's committed payment
//! account, pushed to the seller at `fiat-sent` and returned on the seller's
//! query (`docs/PAYER_HISTORY_ANTI_TRIANGULATION.md` §10.3, §10.4).

use super::db;
use crate::app::context::AppContext;
use crate::config::payer_history::PayerHistorySettings;
use crate::util::{enqueue_order_msg, get_order};
use mostro_core::prelude::*;
use nostr_sdk::prelude::*;
use sqlx::{Pool, Sqlite};

/// Handle the seller's `payment-history` query (§10.4).
///
/// The query takes no parameter beyond the order id: the `(buyer, hash)`
/// pair is resolved server-side, so a seller can only ever learn the history
/// of the account its own counterparty committed to (§11.2). After success
/// the declaration has been consumed and the answer is `not_found`.
pub async fn payment_history_action(
    ctx: &AppContext,
    msg: Message,
    event: &UnwrappedMessage,
    _my_keys: &Keys,
) -> Result<(), MostroError> {
    if !PayerHistorySettings::enabled(ctx.settings().payer_history.as_ref()) {
        return Err(MostroCantDo(CantDoReason::InvalidAction)); // D-10
    }
    let pool = ctx.pool();
    let order = get_order(&msg, pool).await?;
    if order.get_seller_pubkey().ok() != Some(event.sender) {
        return Err(MostroCantDo(CantDoReason::InvalidPeer));
    }
    match order.get_order_status().map_err(MostroInternalErr)? {
        Status::FiatSent | Status::Dispute | Status::SettledHoldInvoice => {}
        Status::Success => return Err(MostroCantDo(CantDoReason::NotFound)),
        _ => return Err(MostroCantDo(CantDoReason::NotAllowedByStatus)),
    }
    let history = build_for_order(pool, &order)
        .await?
        .ok_or(MostroCantDo(CantDoReason::NotFound))?; // buyer never declared
    enqueue_order_msg(
        msg.get_inner_message_kind().request_id,
        Some(order.id),
        Action::PaymentHistory,
        Some(Payload::PaymentHistory(history)),
        event.sender,
        None,
    )
    .await;
    Ok(())
}

/// The history the seller of `order` may see, or `None` when the buyer
/// never declared a payer for it. History is keyed by the buyer's identity
/// key (D-1); a buyer in Full Privacy Mode has none, and gets an honest
/// "unavailable" answer instead of zero counters that read as "new" (D-4).
pub async fn build_for_order(
    pool: &Pool<Sqlite>,
    order: &Order,
) -> Result<Option<PaymentHistory>, MostroError> {
    let Some(declaration) = db::find_declaration(pool, order.id).await? else {
        return Ok(None);
    };
    let (normal_buyer_idkey, _) = order.is_full_privacy_order().map_err(MostroInternalErr)?;
    let Some(user) = normal_buyer_idkey else {
        return Ok(Some(PaymentHistory::unavailable(declaration.payment_hash)));
    };
    let h = db::load_history(pool, &user, &declaration.payment_hash).await?;
    Ok(Some(PaymentHistory {
        payment_hash: declaration.payment_hash,
        buyer_mode: BuyerMode::Reputation,
        successful_trades: h.successful_trades,
        distinct_counterparties: h.distinct_counterparties,
        experienced_counterparties: h.experienced_counterparties,
        first_success_at: h.first_success_at,
        last_success_at: h.last_success_at,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::payer::db::{bump_history, current_policy_generation, upsert_declaration};
    use crate::app::payer::test_support::*;
    use sqlx::SqlitePool;

    fn query(order_id: uuid::Uuid) -> Message {
        Message::new_order(Some(order_id), Some(77), None, Action::PaymentHistory, None)
    }

    async fn run(ctx: &AppContext, sender: PublicKey, msg: Message) -> Result<(), MostroError> {
        payment_history_action(ctx, msg.clone(), &unwrapped(sender, msg), &Keys::generate()).await
    }

    /// Two past successes of `buyer_master` from `h`, with two different
    /// sellers, one of them experienced.
    async fn seed_history(pool: &SqlitePool, buyer_master: &PublicKey, h: &str) {
        let mut conn = pool.acquire().await.unwrap();
        let generation = current_policy_generation(&mut conn, (5, 30), 1)
            .await
            .unwrap();
        let user = buyer_master.to_string();
        bump_history(&mut conn, &user, h, &hash('1'), false, generation, 1_000)
            .await
            .unwrap();
        bump_history(&mut conn, &user, h, &hash('2'), true, generation, 2_000)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn build_for_order_is_none_without_a_declaration() {
        let pool = create_test_pool().await;
        let order = order_in(&pool, Status::FiatSent, Parties::reputation()).await;
        assert!(build_for_order(&pool, &order).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn build_for_order_returns_zero_counters_for_a_new_account() {
        let pool = create_test_pool().await;
        let order = order_in(&pool, Status::FiatSent, Parties::reputation()).await;
        upsert_declaration(&pool, order.id, &hash('a'), 1)
            .await
            .unwrap();

        let h = build_for_order(&pool, &order).await.unwrap().unwrap();
        assert_eq!(h.payment_hash, hash('a'));
        assert_eq!(h.buyer_mode, BuyerMode::Reputation);
        assert_eq!(
            (
                h.successful_trades,
                h.distinct_counterparties,
                h.experienced_counterparties
            ),
            (0, 0, 0)
        );
        assert_eq!((h.first_success_at, h.last_success_at), (None, None));
    }

    #[tokio::test]
    async fn build_for_order_reads_the_buyers_identity_history() {
        let pool = create_test_pool().await;
        let parties = Parties::reputation();
        let order = order_in(&pool, Status::FiatSent, parties).await;
        upsert_declaration(&pool, order.id, &hash('a'), 1)
            .await
            .unwrap();
        seed_history(&pool, &parties.buyer_master, &hash('a')).await;
        // The trade key has no history of its own: continuity is the
        // identity key (D-1).
        seed_history(&pool, &parties.buyer, &hash('b')).await;

        let h = build_for_order(&pool, &order).await.unwrap().unwrap();
        assert_eq!(h.buyer_mode, BuyerMode::Reputation);
        assert_eq!(
            (
                h.successful_trades,
                h.distinct_counterparties,
                h.experienced_counterparties
            ),
            (2, 2, 1)
        );
        assert_eq!(
            (h.first_success_at, h.last_success_at),
            (Some(1_000), Some(2_000))
        );
    }

    #[tokio::test]
    async fn build_for_order_reports_full_privacy_buyers_as_unavailable() {
        let pool = create_test_pool().await;
        let parties = Parties::full_privacy_buyer();
        let order = order_in(&pool, Status::FiatSent, parties).await;
        upsert_declaration(&pool, order.id, &hash('a'), 1)
            .await
            .unwrap();
        // Even if rows existed under that key, they must not be shown (D-4).
        seed_history(&pool, &parties.buyer, &hash('a')).await;

        let h = build_for_order(&pool, &order).await.unwrap().unwrap();
        assert_eq!(h, PaymentHistory::unavailable(hash('a')));
    }

    #[tokio::test]
    async fn query_with_the_feature_off_is_invalid_action() {
        let pool = create_test_pool().await;
        let parties = Parties::reputation();
        let order = order_in(&pool, Status::FiatSent, parties).await;
        upsert_declaration(&pool, order.id, &hash('a'), 1)
            .await
            .unwrap();

        let ctx = ctx_with(&pool, Some(payer_settings(false, false)));
        assert_cant_do(
            run(&ctx, parties.seller, query(order.id)).await,
            CantDoReason::InvalidAction,
        );
        let ctx = ctx_with(&pool, None);
        assert_cant_do(
            run(&ctx, parties.seller, query(order.id)).await,
            CantDoReason::InvalidAction,
        );
        assert!(queued_for(order.id).await.is_empty());
    }

    #[tokio::test]
    async fn only_the_seller_may_query() {
        let pool = create_test_pool().await;
        let ctx = ctx_with(&pool, Some(payer_settings(true, false)));
        let parties = Parties::reputation();
        let order = order_in(&pool, Status::FiatSent, parties).await;
        upsert_declaration(&pool, order.id, &hash('a'), 1)
            .await
            .unwrap();

        for intruder in [
            parties.buyer,
            parties.seller_master,
            Keys::generate().public_key(),
        ] {
            assert_cant_do(
                run(&ctx, intruder, query(order.id)).await,
                CantDoReason::InvalidPeer,
            );
        }
        assert!(queued_for(order.id).await.is_empty());
    }

    #[tokio::test]
    async fn query_is_allowed_only_between_fiat_sent_and_settlement() {
        let pool = create_test_pool().await;
        let ctx = ctx_with(&pool, Some(payer_settings(true, false)));
        let parties = Parties::reputation();

        for status in [
            Status::FiatSent,
            Status::Dispute,
            Status::SettledHoldInvoice,
        ] {
            let order = order_in(&pool, status, parties).await;
            upsert_declaration(&pool, order.id, &hash('a'), 1)
                .await
                .unwrap();
            let res = run(&ctx, parties.seller, query(order.id)).await;
            assert!(res.is_ok(), "{status:?} must answer: {res:?}");
        }
        for status in [
            Status::Active,
            Status::WaitingPayment,
            Status::Pending,
            Status::Canceled,
        ] {
            let order = order_in(&pool, status, parties).await;
            upsert_declaration(&pool, order.id, &hash('a'), 1)
                .await
                .unwrap();
            assert_cant_do(
                run(&ctx, parties.seller, query(order.id)).await,
                CantDoReason::NotAllowedByStatus,
            );
        }
        // After success the declaration has been consumed (§10.4).
        let done = order_in(&pool, Status::Success, parties).await;
        assert_cant_do(
            run(&ctx, parties.seller, query(done.id)).await,
            CantDoReason::NotFound,
        );
    }

    #[tokio::test]
    async fn query_without_a_declaration_is_not_found() {
        let pool = create_test_pool().await;
        let ctx = ctx_with(&pool, Some(payer_settings(true, false)));
        let parties = Parties::reputation();
        let order = order_in(&pool, Status::FiatSent, parties).await;

        assert_cant_do(
            run(&ctx, parties.seller, query(order.id)).await,
            CantDoReason::NotFound,
        );
    }

    #[tokio::test]
    async fn query_answers_the_seller_with_the_request_id() {
        let pool = create_test_pool().await;
        let ctx = ctx_with(&pool, Some(payer_settings(true, false)));
        let parties = Parties::reputation();
        let order = order_in(&pool, Status::FiatSent, parties).await;
        upsert_declaration(&pool, order.id, &hash('a'), 1)
            .await
            .unwrap();
        seed_history(&pool, &parties.buyer_master, &hash('a')).await;

        run(&ctx, parties.seller, query(order.id)).await.unwrap();

        let queued = queued_for(order.id).await;
        assert_eq!(queued.len(), 1);
        let reply = &queued[0];
        assert_eq!(reply.action, Action::PaymentHistory);
        assert_eq!(reply.request_id, Some(77));
        assert_eq!(reply.destination, parties.seller);
        let h = reply.history().expect("payment_history payload");
        assert_eq!((h.successful_trades, h.distinct_counterparties), (2, 2));
    }
}
