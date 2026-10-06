//! The success hook: record a finished trade in the buyer's payment-account
//! history (`docs/PAYER_HISTORY_ANTI_TRIANGULATION.md` §10.5, D-5).

use super::{counterparty_id, db, is_experienced};
use mostro_core::prelude::*;
use nostr_sdk::prelude::*;
use sqlx::SqliteConnection;

/// Record `order`'s success in the payer history. Runs inside the Success
/// CAS transaction of `release::payment_success`, only on the branch that
/// performed the transition, in a savepoint of its own: a failure here
/// undoes this function's writes and the Success still commits.
///
/// The declaration row is the idempotency token: it is taken (deleted)
/// first, so at most one increment can ever happen per order. Disputed
/// trades (D-6) and Full Privacy buyers (D-4) consume it without recording
/// anything.
pub async fn record_payer_success(
    conn: &mut SqliteConnection,
    node_keys: &Keys,
    order: &Order,
    thresholds: (u32, u32),
    now: i64,
) -> Result<(), MostroError> {
    let Some(declaration) = db::take_declaration(conn, order.id).await? else {
        return Ok(()); // nothing declared, or already recorded
    };
    if order.buyer_dispute || order.seller_dispute {
        return Ok(()); // D-6: a trade that needed a solver is not evidence
    }
    let (buyer_idkey, _) = order.is_full_privacy_order().map_err(MostroInternalErr)?;
    let Some(user) = buyer_idkey else {
        return Ok(()); // D-4: no continuity to record
    };
    let seller_master = order
        .get_master_seller_pubkey()
        .map(|k| k.to_string())
        .unwrap_or_else(|_| order.seller_pubkey.clone().unwrap_or_default());
    let cp = counterparty_id(node_keys, &seller_master);
    // D-7 snapshot. The CAS has already flipped this order to Success inside
    // the transaction, so it is excluded explicitly: the recorded trade never
    // counts toward its own counterparty's qualification. Bounded to `now`,
    // the instant stamped on this order, like the recompute: a success
    // stamped later but committed first must not count toward this one.
    let experience =
        db::seller_experience(conn, &seller_master, &user, Some(order.id), Some(now)).await?;
    let experienced = is_experienced(&experience, thresholds.0, thresholds.1, now);
    let generation = db::current_policy_generation(conn, node_keys, thresholds, now).await?;
    db::bump_history(
        conn,
        node_keys,
        &user,
        &declaration.payment_hash,
        &cp,
        experienced,
        generation,
        now,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::payer::counterparty_id;
    use crate::app::payer::db::{find_declaration, load_history, upsert_declaration};
    use crate::app::payer::test_support::*;
    use mostro_core::db::Crud;
    use sqlx::SqlitePool;

    const ONE_DAY: i64 = 86_400;
    const NOW: i64 = 3_000 * ONE_DAY;
    const THRESHOLDS: (u32, u32) = (2, 30);

    /// A trade between `parties` that just reached Success, with the buyer's
    /// declaration of `h` still pending.
    async fn finished_trade(pool: &SqlitePool, parties: Parties, h: &str) -> Order {
        let order = order_in(pool, Status::Success, parties).await;
        upsert_declaration(pool, order.id, h, NOW - 60)
            .await
            .unwrap();
        order
    }

    /// An earlier undisputed success of `seller_master` with some other buyer.
    async fn prior_success(pool: &SqlitePool, seller_master: PublicKey, created_at: i64) {
        Order {
            id: uuid::Uuid::new_v4(),
            status: Status::Success.to_string(),
            kind: mostro_core::order::Kind::Sell.to_string(),
            fiat_code: "USD".to_string(),
            creator_pubkey: seller_master.to_string(),
            seller_pubkey: Some(Keys::generate().public_key().to_string()),
            master_seller_pubkey: Some(seller_master.to_string()),
            buyer_pubkey: Some(Keys::generate().public_key().to_string()),
            master_buyer_pubkey: Some(Keys::generate().public_key().to_string()),
            amount: 1_000,
            fiat_amount: 1,
            created_at,
            ..Default::default()
        }
        .create(pool)
        .await
        .unwrap();
    }

    async fn record(pool: &SqlitePool, node: &Keys, order: &Order, now: i64) {
        let mut tx = pool.begin().await.unwrap();
        record_payer_success(&mut tx, node, order, THRESHOLDS, now)
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }

    async fn history(pool: &SqlitePool, parties: Parties, h: &str) -> (u32, u32, u32) {
        let c = load_history(pool, node_keys(), &parties.buyer_master.to_string(), h)
            .await
            .unwrap();
        (
            c.successful_trades,
            c.distinct_counterparties,
            c.experienced_counterparties,
        )
    }

    async fn rows(pool: &SqlitePool) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM payer_history")
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn first_success_creates_the_history_and_consumes_the_declaration() {
        let pool = create_test_pool().await;
        let node = node_keys().clone();
        let parties = Parties::reputation();
        let order = finished_trade(&pool, parties, &hash('a')).await;

        record(&pool, &node, &order, NOW).await;

        assert_eq!(history(&pool, parties, &hash('a')).await, (1, 1, 0));
        let c = load_history(
            &pool,
            node_keys(),
            &parties.buyer_master.to_string(),
            &hash('a'),
        )
        .await
        .unwrap();
        assert_eq!(
            (c.first_success_at, c.last_success_at),
            (Some(NOW), Some(NOW))
        );
        assert!(find_declaration(&pool, order.id).await.unwrap().is_none());
        // The counterparty is stored as the keyed hash of the seller's
        // identity key, never as a pubkey (D-7).
        let cp: String =
            sqlx::query_scalar("SELECT counterparty_id FROM payer_history_counterparties")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            cp,
            counterparty_id(&node, &parties.seller_master.to_string())
        );
    }

    #[tokio::test]
    async fn a_second_call_for_the_same_order_is_a_no_op() {
        let pool = create_test_pool().await;
        let node = node_keys().clone();
        let parties = Parties::reputation();
        let order = finished_trade(&pool, parties, &hash('a')).await;

        record(&pool, &node, &order, NOW).await;
        record(&pool, &node, &order, NOW + 1).await;

        assert_eq!(history(&pool, parties, &hash('a')).await, (1, 1, 0));
    }

    #[tokio::test]
    async fn without_a_declaration_nothing_is_recorded() {
        let pool = create_test_pool().await;
        let order = order_in(&pool, Status::Success, Parties::reputation()).await;
        record(&pool, node_keys(), &order, NOW).await;
        assert_eq!(rows(&pool).await, 0);
    }

    #[tokio::test]
    async fn distinct_counterparties_count_sellers_not_trades() {
        let pool = create_test_pool().await;
        let node = node_keys().clone();
        let parties = Parties::reputation();
        let mut other_seller = Parties::reputation();
        other_seller.buyer = parties.buyer;
        other_seller.buyer_master = parties.buyer_master;

        for p in [parties, parties, other_seller] {
            let order = finished_trade(&pool, p, &hash('a')).await;
            record(&pool, &node, &order, NOW).await;
        }

        assert_eq!(history(&pool, parties, &hash('a')).await, (3, 2, 0));
    }

    #[tokio::test]
    async fn disputed_trades_are_discarded() {
        let pool = create_test_pool().await;
        let parties = Parties::reputation();
        let mut order = finished_trade(&pool, parties, &hash('a')).await;
        order.buyer_dispute = true;

        record(&pool, node_keys(), &order, NOW).await;

        assert_eq!(rows(&pool).await, 0, "D-6");
        assert!(
            find_declaration(&pool, order.id).await.unwrap().is_none(),
            "consumed"
        );
    }

    #[tokio::test]
    async fn full_privacy_buyers_store_nothing() {
        let pool = create_test_pool().await;
        let order = finished_trade(&pool, Parties::full_privacy_buyer(), &hash('a')).await;

        record(&pool, node_keys(), &order, NOW).await;

        assert_eq!(rows(&pool).await, 0, "D-4");
        assert!(
            find_declaration(&pool, order.id).await.unwrap().is_none(),
            "consumed"
        );
    }

    #[tokio::test]
    async fn an_experienced_seller_is_flagged() {
        let pool = create_test_pool().await;
        let parties = Parties::reputation();
        prior_success(&pool, parties.seller_master, NOW - 40 * ONE_DAY).await;
        prior_success(&pool, parties.seller_master, NOW - 35 * ONE_DAY).await;
        let order = finished_trade(&pool, parties, &hash('a')).await;

        record(&pool, node_keys(), &order, NOW).await;

        assert_eq!(history(&pool, parties, &hash('a')).await, (1, 1, 1));
    }

    #[tokio::test]
    async fn successes_stamped_after_the_recorded_one_do_not_qualify_the_seller() {
        // A trade stamped at NOW whose hook runs after another buyer's trade
        // stamped later: that later trade must not count toward NOW.
        let pool = create_test_pool().await;
        let parties = Parties::reputation();
        prior_success(&pool, parties.seller_master, NOW - 40 * ONE_DAY).await;
        prior_success(&pool, parties.seller_master, NOW - 35 * ONE_DAY).await;
        sqlx::query(
            "UPDATE orders SET success_at = ?1 \
              WHERE id = (SELECT id FROM orders WHERE created_at = ?2)",
        )
        .bind(NOW + 100)
        .bind(NOW - 35 * ONE_DAY)
        .execute(&pool)
        .await
        .unwrap();
        let order = finished_trade(&pool, parties, &hash('a')).await;

        record(&pool, node_keys(), &order, NOW).await;

        assert_eq!(
            history(&pool, parties, &hash('a')).await,
            (1, 1, 0),
            "only one success precedes NOW"
        );
    }

    #[tokio::test]
    async fn a_seller_below_either_threshold_is_not_flagged() {
        let pool = create_test_pool().await;
        // One trade short.
        let few = Parties::reputation();
        prior_success(&pool, few.seller_master, NOW - 40 * ONE_DAY).await;
        // Enough trades, too young.
        let young = Parties::reputation();
        prior_success(&pool, young.seller_master, NOW - 10 * ONE_DAY).await;
        prior_success(&pool, young.seller_master, NOW - 5 * ONE_DAY).await;

        for p in [few, young] {
            let order = finished_trade(&pool, p, &hash('a')).await;
            record(&pool, node_keys(), &order, NOW).await;
            assert_eq!(history(&pool, p, &hash('a')).await, (1, 1, 0));
        }
    }

    #[tokio::test]
    async fn the_recorded_trade_never_counts_toward_its_own_qualification() {
        // One prior success plus the trade being recorded would reach N = 2.
        let pool = create_test_pool().await;
        let parties = Parties::reputation();
        prior_success(&pool, parties.seller_master, NOW - 40 * ONE_DAY).await;
        let order = finished_trade(&pool, parties, &hash('a')).await;

        record(&pool, node_keys(), &order, NOW).await;

        assert_eq!(history(&pool, parties, &hash('a')).await, (1, 1, 0));
    }

    #[tokio::test]
    async fn trades_with_the_same_buyer_never_qualify_the_seller() {
        let pool = create_test_pool().await;
        let node = node_keys().clone();
        let parties = Parties::reputation();
        // Many old successes, all with this very buyer (two-key Sybil).
        for _ in 0..5 {
            let past = finished_trade(&pool, parties, &hash('b')).await;
            sqlx::query("UPDATE orders SET created_at = ?1 WHERE id = ?2")
                .bind(NOW - 90 * ONE_DAY)
                .bind(past.id)
                .execute(&pool)
                .await
                .unwrap();
        }
        let order = finished_trade(&pool, parties, &hash('a')).await;

        record(&pool, &node, &order, NOW).await;

        assert_eq!(history(&pool, parties, &hash('a')).await, (1, 1, 0));
    }

    #[tokio::test]
    async fn disputed_past_trades_do_not_qualify_the_seller() {
        let pool = create_test_pool().await;
        let parties = Parties::reputation();
        prior_success(&pool, parties.seller_master, NOW - 40 * ONE_DAY).await;
        prior_success(&pool, parties.seller_master, NOW - 35 * ONE_DAY).await;
        sqlx::query("UPDATE orders SET seller_dispute = 1 WHERE master_seller_pubkey = ?1")
            .bind(parties.seller_master.to_string())
            .execute(&pool)
            .await
            .unwrap();
        let order = finished_trade(&pool, parties, &hash('a')).await;

        record(&pool, node_keys(), &order, NOW).await;

        assert_eq!(history(&pool, parties, &hash('a')).await, (1, 1, 0));
    }

    #[tokio::test]
    async fn the_flag_upgrades_on_a_later_success_and_never_downgrades() {
        let pool = create_test_pool().await;
        let node = node_keys().clone();
        let parties = Parties::reputation();

        // First trade: the seller is new.
        let first = finished_trade(&pool, parties, &hash('a')).await;
        record(&pool, &node, &first, NOW).await;
        assert_eq!(history(&pool, parties, &hash('a')).await, (1, 1, 0));

        // The seller crosses the thresholds with other buyers afterwards:
        // the stored snapshot is NOT re-evaluated (no retroactivity)...
        prior_success(&pool, parties.seller_master, NOW - 40 * ONE_DAY).await;
        prior_success(&pool, parties.seller_master, NOW - 35 * ONE_DAY).await;
        assert_eq!(history(&pool, parties, &hash('a')).await, (1, 1, 0));

        // ...until another success with the same triple lands.
        let second = finished_trade(&pool, parties, &hash('a')).await;
        record(&pool, &node, &second, NOW).await;
        assert_eq!(history(&pool, parties, &hash('a')).await, (2, 1, 1));

        // Disputes on the seller's record later cannot lower it.
        sqlx::query("UPDATE orders SET seller_dispute = 1 WHERE master_seller_pubkey = ?1 AND master_buyer_pubkey <> ?2")
            .bind(parties.seller_master.to_string())
            .bind(parties.buyer_master.to_string())
            .execute(&pool)
            .await
            .unwrap();
        let third = finished_trade(&pool, parties, &hash('a')).await;
        record(&pool, &node, &third, NOW).await;
        assert_eq!(
            history(&pool, parties, &hash('a')).await,
            (3, 1, 1),
            "MAX never flips back"
        );
    }

    #[tokio::test]
    async fn without_a_seller_identity_key_the_trade_key_identifies_the_counterparty() {
        let pool = create_test_pool().await;
        let node = node_keys().clone();
        let parties = Parties::reputation();
        let mut order = finished_trade(&pool, parties, &hash('a')).await;
        order.master_seller_pubkey = None;

        record(&pool, &node, &order, NOW).await;

        let cp: String =
            sqlx::query_scalar("SELECT counterparty_id FROM payer_history_counterparties")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(cp, counterparty_id(&node, &parties.seller.to_string()));
    }
}
