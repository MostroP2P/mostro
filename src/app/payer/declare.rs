//! `declare-payer`: the buyer commits to the fiat account it will pay from
//! (`docs/PAYER_HISTORY_ANTI_TRIANGULATION.md` §10.2).

use super::{db, validate_payment_hash};
use crate::app::context::AppContext;
use crate::config::payer_history::PayerHistorySettings;
use crate::util::{enqueue_order_msg, get_order};
use mostro_core::prelude::*;
use nostr_sdk::prelude::*;

/// Handle `declare-payer` (buyer → Mostro).
///
/// Stores the buyer's hash-only commitment for the order (last write wins),
/// acks it to the buyer and forwards it to the seller so the seller's client
/// can compare it with the hash of the plaintext it receives off-band
/// (D-2, D-3). The declaration is frozen once fiat is reported sent.
pub async fn declare_payer_action(
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
    if order.get_buyer_pubkey().ok() != Some(event.sender) {
        return Err(MostroCantDo(CantDoReason::InvalidPubkey));
    }
    let status = order.get_order_status().map_err(MostroInternalErr)?;
    if !matches!(
        status,
        Status::WaitingPayment | Status::WaitingBuyerInvoice | Status::Active
    ) {
        return Err(MostroCantDo(CantDoReason::NotAllowedByStatus));
    }
    let declaration = match msg.get_inner_message_kind().get_payload() {
        Some(Payload::PayerDeclaration(d)) => d.clone(),
        _ => return Err(MostroCantDo(CantDoReason::InvalidPayload)),
    };
    validate_payment_hash(&declaration.payment_hash)?;
    // Conditional on the status again, inside the write: the check above
    // gives the precise refusal, this one closes the race with a
    // concurrent `fiat-sent`.
    let stored = db::upsert_open_declaration(
        pool,
        order.id,
        &declaration.payment_hash,
        Timestamp::now().as_secs() as i64,
    )
    .await?;
    if !stored {
        return Err(MostroCantDo(CantDoReason::NotAllowedByStatus));
    }

    let payload = Some(Payload::PayerDeclaration(declaration));
    enqueue_order_msg(
        msg.get_inner_message_kind().request_id,
        Some(order.id),
        Action::PayerDeclared,
        payload.clone(),
        event.sender,
        None,
    )
    .await;
    // A maker-buyer order may have no seller yet; the seller then learns the
    // hash from the `payment-history` push at `fiat-sent`.
    if let Ok(seller) = order.get_seller_pubkey() {
        enqueue_order_msg(
            None,
            Some(order.id),
            Action::PayerDeclared,
            payload,
            seller,
            None,
        )
        .await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::context::test_utils::{test_settings, TestContextBuilder};
    use crate::app::payer::db::find_declaration;
    use crate::config::payer_history::PayerHistorySettings;
    use crate::config::MESSAGE_QUEUES;
    use mostro_core::db::Crud;
    use sqlx::SqlitePool;
    use std::sync::Arc;

    async fn create_test_pool() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!().run(&pool).await.unwrap();
        pool
    }

    fn ctx_with(pool: &SqlitePool, enabled: bool) -> AppContext {
        let mut settings = test_settings();
        settings.payer_history = Some(PayerHistorySettings {
            enabled,
            ..Default::default()
        });
        TestContextBuilder::new()
            .with_pool(Arc::new(pool.clone()))
            .with_settings(settings)
            .build()
    }

    fn hash(c: char) -> String {
        std::iter::repeat_n(c, 64).collect()
    }

    fn unwrapped(sender: PublicKey, msg: Message) -> UnwrappedMessage {
        UnwrappedMessage {
            message: msg,
            signature: None,
            sender,
            identity: Keys::generate().public_key(),
            created_at: Timestamp::now(),
        }
    }

    fn declare_msg(order_id: uuid::Uuid, payload: Option<Payload>) -> Message {
        Message::new_order(
            Some(order_id),
            Some(42),
            None,
            Action::DeclarePayer,
            payload,
        )
    }

    fn declaration(h: &str) -> Option<Payload> {
        Some(Payload::PayerDeclaration(PayerDeclaration::new(
            h.to_string(),
        )))
    }

    async fn order_in(
        pool: &SqlitePool,
        status: Status,
        seller: Option<PublicKey>,
        buyer: PublicKey,
    ) -> Order {
        Order {
            id: uuid::Uuid::new_v4(),
            status: status.to_string(),
            kind: mostro_core::order::Kind::Sell.to_string(),
            fiat_code: "USD".to_string(),
            creator_pubkey: buyer.to_string(),
            seller_pubkey: seller.map(|s| s.to_string()),
            master_seller_pubkey: seller.map(|s| s.to_string()),
            buyer_pubkey: Some(buyer.to_string()),
            master_buyer_pubkey: Some(buyer.to_string()),
            amount: 21_000,
            fiat_amount: 40,
            ..Default::default()
        }
        .create(pool)
        .await
        .unwrap()
    }

    /// `(action, request_id, destination, declared hash)` queued for
    /// `order_id`. The queue is process-global, so always filter by our own
    /// order id.
    async fn queued_for(
        order_id: uuid::Uuid,
    ) -> Vec<(Action, Option<u64>, PublicKey, Option<String>)> {
        MESSAGE_QUEUES
            .queue_order_msg
            .read()
            .await
            .iter()
            .filter(|(msg, _)| msg.get_inner_message_kind().id == Some(order_id))
            .map(|(msg, dest)| {
                let k = msg.get_inner_message_kind();
                let declared = match &k.payload {
                    Some(Payload::PayerDeclaration(d)) => Some(d.payment_hash.clone()),
                    _ => None,
                };
                (k.action.clone(), k.request_id, *dest, declared)
            })
            .collect()
    }

    async fn run(ctx: &AppContext, sender: PublicKey, msg: Message) -> Result<(), MostroError> {
        declare_payer_action(ctx, msg.clone(), &unwrapped(sender, msg), &Keys::generate()).await
    }

    fn assert_cant_do(res: Result<(), MostroError>, expected: CantDoReason) {
        match res {
            Err(MostroCantDo(reason)) => assert_eq!(reason, expected),
            other => panic!("expected cant-do {expected:?}, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn feature_off_answers_invalid_action_and_stores_nothing() {
        let pool = create_test_pool().await;
        let (seller, buyer) = (Keys::generate().public_key(), Keys::generate().public_key());
        let order = order_in(&pool, Status::Active, Some(seller), buyer).await;

        let res = run(
            &ctx_with(&pool, false),
            buyer,
            declare_msg(order.id, declaration(&hash('a'))),
        )
        .await;

        assert_cant_do(res, CantDoReason::InvalidAction);
        assert!(find_declaration(&pool, order.id).await.unwrap().is_none());
        assert!(queued_for(order.id).await.is_empty());
    }

    #[tokio::test]
    async fn unknown_order_is_not_found() {
        let pool = create_test_pool().await;
        let res = run(
            &ctx_with(&pool, true),
            Keys::generate().public_key(),
            declare_msg(uuid::Uuid::new_v4(), declaration(&hash('a'))),
        )
        .await;
        assert_cant_do(res, CantDoReason::NotFound);
    }

    #[tokio::test]
    async fn only_the_buyer_may_declare() {
        let pool = create_test_pool().await;
        let ctx = ctx_with(&pool, true);
        let (seller, buyer) = (Keys::generate().public_key(), Keys::generate().public_key());
        let order = order_in(&pool, Status::Active, Some(seller), buyer).await;

        for intruder in [seller, Keys::generate().public_key()] {
            let res = run(
                &ctx,
                intruder,
                declare_msg(order.id, declaration(&hash('a'))),
            )
            .await;
            assert_cant_do(res, CantDoReason::InvalidPubkey);
        }
        assert!(find_declaration(&pool, order.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn declaration_window_is_waiting_or_active_only() {
        let pool = create_test_pool().await;
        let ctx = ctx_with(&pool, true);
        let (seller, buyer) = (Keys::generate().public_key(), Keys::generate().public_key());

        for status in [
            Status::WaitingPayment,
            Status::WaitingBuyerInvoice,
            Status::Active,
        ] {
            let order = order_in(&pool, status, Some(seller), buyer).await;
            let res = run(&ctx, buyer, declare_msg(order.id, declaration(&hash('a')))).await;
            assert!(res.is_ok(), "{status:?} must accept a declaration: {res:?}");
        }
        for status in [
            Status::Pending,
            Status::FiatSent,
            Status::SettledHoldInvoice,
            Status::Success,
            Status::Dispute,
            Status::Canceled,
        ] {
            let order = order_in(&pool, status, Some(seller), buyer).await;
            let res = run(&ctx, buyer, declare_msg(order.id, declaration(&hash('a')))).await;
            assert_cant_do(res, CantDoReason::NotAllowedByStatus);
            assert!(find_declaration(&pool, order.id).await.unwrap().is_none());
        }
    }

    #[tokio::test]
    async fn malformed_hash_and_wrong_payload_are_rejected() {
        let pool = create_test_pool().await;
        let ctx = ctx_with(&pool, true);
        let (seller, buyer) = (Keys::generate().public_key(), Keys::generate().public_key());
        let order = order_in(&pool, Status::Active, Some(seller), buyer).await;

        for bad in ["abc".to_string(), hash('A'), hash('z')] {
            let res = run(&ctx, buyer, declare_msg(order.id, declaration(&bad))).await;
            assert_cant_do(res, CantDoReason::InvalidPaymentHash);
        }
        for payload in [None, Some(Payload::TextMessage("plaintext".to_string()))] {
            let res = run(&ctx, buyer, declare_msg(order.id, payload)).await;
            assert_cant_do(res, CantDoReason::InvalidPayload);
        }
        assert!(find_declaration(&pool, order.id).await.unwrap().is_none());
        assert!(queued_for(order.id).await.is_empty());
    }

    #[tokio::test]
    async fn declaration_is_stored_acked_to_buyer_and_forwarded_to_seller() {
        let pool = create_test_pool().await;
        let ctx = ctx_with(&pool, true);
        let (seller, buyer) = (Keys::generate().public_key(), Keys::generate().public_key());
        let order = order_in(&pool, Status::Active, Some(seller), buyer).await;

        run(&ctx, buyer, declare_msg(order.id, declaration(&hash('a'))))
            .await
            .unwrap();

        let stored = find_declaration(&pool, order.id).await.unwrap().unwrap();
        assert_eq!(stored.payment_hash, hash('a'));
        let queued = queued_for(order.id).await;
        assert_eq!(
            queued,
            vec![
                (Action::PayerDeclared, Some(42), buyer, Some(hash('a'))),
                (Action::PayerDeclared, None, seller, Some(hash('a'))),
            ]
        );
    }

    #[tokio::test]
    async fn redeclaration_overwrites_and_notifies_again() {
        let pool = create_test_pool().await;
        let ctx = ctx_with(&pool, true);
        let (seller, buyer) = (Keys::generate().public_key(), Keys::generate().public_key());
        let order = order_in(&pool, Status::Active, Some(seller), buyer).await;

        run(&ctx, buyer, declare_msg(order.id, declaration(&hash('a'))))
            .await
            .unwrap();
        run(&ctx, buyer, declare_msg(order.id, declaration(&hash('b'))))
            .await
            .unwrap();

        let stored = find_declaration(&pool, order.id).await.unwrap().unwrap();
        assert_eq!(stored.payment_hash, hash('b'));
        let to_seller: Vec<_> = queued_for(order.id)
            .await
            .into_iter()
            .filter(|(_, _, dest, _)| *dest == seller)
            .map(|(_, _, _, payload)| payload)
            .collect();
        assert_eq!(to_seller, vec![Some(hash('a')), Some(hash('b'))]);
    }

    #[tokio::test]
    async fn without_a_seller_only_the_buyer_is_acked() {
        // Maker-buyer order still waiting for its taker to pay: no seller yet.
        let pool = create_test_pool().await;
        let ctx = ctx_with(&pool, true);
        let buyer = Keys::generate().public_key();
        let order = order_in(&pool, Status::WaitingPayment, None, buyer).await;

        run(&ctx, buyer, declare_msg(order.id, declaration(&hash('c'))))
            .await
            .unwrap();

        let queued = queued_for(order.id).await;
        assert_eq!(
            queued,
            vec![(Action::PayerDeclared, Some(42), buyer, Some(hash('c')))]
        );
        assert!(find_declaration(&pool, order.id).await.unwrap().is_some());
    }
}
