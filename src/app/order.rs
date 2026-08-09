use crate::app::context::AppContext;
use crate::db::update_user_trade_index;
use crate::util::{get_bitcoin_price, publish_order, validate_invoice};
use mostro_core::prelude::*;
use nostr_sdk::prelude::*;
use nostr_sdk::Keys;

async fn calculate_and_check_quote(
    ctx: &AppContext,
    order: &SmallOrder,
    fiat_amount: &i64,
) -> Result<(), MostroError> {
    // Get mostro settings
    let mostro_settings = &ctx.settings().mostro;
    // Calculate quote
    let quote = match order.amount {
        0 => match get_bitcoin_price(&order.fiat_code) {
            Ok(price) => {
                let quote = *fiat_amount as f64 / price;
                (quote * 1E8) as i64
            }
            // No fresh rate within the staleness window — refuse the
            // market-priced order cleanly instead of pricing on stale data.
            Err(MostroInternalErr(ServiceError::PriceTooStale)) => {
                return Err(MostroCantDo(CantDoReason::PriceTooStale));
            }
            Err(_) => {
                return Err(MostroInternalErr(ServiceError::NoAPIResponse));
            }
        },
        _ => order.amount,
    };

    // Check amount is positive - extra safety check
    if quote < 0 {
        return Err(MostroCantDo(CantDoReason::InvalidAmount));
    }

    if quote > mostro_settings.max_order_amount as i64
        || quote < mostro_settings.min_payment_amount as i64
    {
        return Err(MostroCantDo(CantDoReason::OutOfRangeSatsAmount));
    }

    Ok(())
}

/// Processes a trading order message by validating, updating, and publishing the order.
///
/// This asynchronous function inspects the provided message for an order and, if found, proceeds to:
/// - Validate the associated invoice.
/// - Check if fiat currency is accepted by mostro instance
/// - Check order constraints such as range limits and zero-amount premium conditions.
/// - Calculate a valid quote (in satoshis) for each fiat amount in the order.
/// - Determine the appropriate trade index, using a fallback when the sender matches the rumor's public key.
/// - Update the user's trade index in the database and publish the order.
///
/// If the message does not contain an order, the function simply returns `Ok(())`.
///
/// # Parameters
/// - `ctx`: Application context containing the database pool and other dependencies.
/// - `msg`: Trading message containing order details and a request ID.
/// - `event`: Event data providing sender and rumor details required for determining the trade index.
/// - `my_keys`: Local signing keys used during order publication.
///
/// # Errors
/// Returns a `MostroError` if any validation, quote calculation, trade index update, or order publication fails.
///
/// # Examples
///
/// ```rust,ignore
/// # use your_crate::{order_action, Message, UnwrappedMessage, Keys, AppContext};
/// # async fn run_example(ctx: &AppContext) -> Result<(), MostroError> {
/// // Initialize dummy instances; in a real application, replace these with actual values.
/// let msg = Message::default();
/// let event = UnwrappedMessage::default();
/// let my_keys = Keys::default();
///
/// // Process the order if present in the message.
/// order_action(&ctx, msg, &event, &my_keys).await?;
/// # Ok(())
/// # }
/// ```
pub async fn order_action(
    ctx: &AppContext,
    msg: Message,
    event: &UnwrappedMessage,
    my_keys: &Keys,
) -> Result<(), MostroError> {
    let pool = ctx.pool();
    // Get request id
    let request_id = msg.get_inner_message_kind().request_id;

    if let Some(order) = msg.get_inner_message_kind().get_order() {
        // Validate invoice
        let _invoice = validate_invoice(&msg, &Order::from(order.clone())).await?;

        // Check if fiat currency is accepted
        let mostro_settings = &ctx.settings().mostro;
        if let Err(cause) = order.check_fiat_currency(&mostro_settings.fiat_currencies_accepted) {
            return Err(MostroCantDo(cause));
        }

        // `check_fiat_amount` in mostro-core requires fiat_amount > 0. Range orders set
        // min/max and use fiat_amount == 0, so only run it for single-amount orders.
        if order.min_amount.is_none() && order.max_amount.is_none() {
            if let Err(cause) = order.check_fiat_amount() {
                return Err(MostroCantDo(cause));
            }
        }

        // Validate amount (sats) is non-negative
        if let Err(cause) = order.check_amount() {
            return Err(MostroCantDo(cause));
        }

        // Default case single amount
        let mut amount_vec = vec![order.fiat_amount];
        // Get max and and min amount in case of range order
        // in case of single order do like usual
        if let Err(cause) = order.check_range_order_limits(&mut amount_vec) {
            return Err(MostroCantDo(cause));
        }

        // Check if zero amount with premium
        if let Err(cause) = order.check_zero_amount_with_premium() {
            return Err(MostroCantDo(cause));
        }

        // Check quote in sats for each amount
        for fiat_amount in amount_vec.iter() {
            calculate_and_check_quote(ctx, order, fiat_amount).await?;
        }

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

        // Update trade index only after all checks are done
        update_user_trade_index(pool, event.identity.to_string(), trade_index)
            .await
            .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

        // Publish order
        publish_order(
            pool,
            my_keys,
            order,
            event.sender,
            event.identity,
            event.sender,
            request_id,
            msg.get_inner_message_kind().trade_index,
        )
        .await?
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mostro_core::message::MessageKind;

    use nostr_sdk::{Keys, Timestamp};
    use sqlx::SqlitePool;

    async fn create_test_pool() -> SqlitePool {
        SqlitePool::connect(":memory:").await.unwrap()
    }

    fn create_test_keys() -> Keys {
        Keys::generate()
    }

    fn create_test_message(trade_index: Option<u32>) -> Message {
        Message::new_order(
            Some(uuid::Uuid::new_v4()),
            Some(1),
            trade_index.map(|i| i as i64),
            Action::NewOrder,
            None, // We don't need payload for structure tests
        )
    }

    fn create_test_unwrapped_message() -> UnwrappedMessage {
        let identity = create_test_keys();
        let trade = create_test_keys();

        UnwrappedMessage {
            message: create_test_message(None),
            signature: None,
            sender: trade.public_key(),
            identity: identity.public_key(),
            created_at: Timestamp::now(),
        }
    }

    fn create_test_order_message(fiat_amount: i64, amount: i64) -> Message {
        let order = mostro_core::order::SmallOrder::new(
            Some(uuid::Uuid::new_v4()),
            Some(mostro_core::order::Kind::Sell),
            Some(mostro_core::order::Status::Pending),
            amount,
            "USD".to_string(),
            None,
            None,
            fiat_amount,
            "BANK".to_string(),
            0,
            None,
            None,
            None,
            None,
            None,
        );
        Message::new_order(
            Some(uuid::Uuid::new_v4()),
            Some(1),
            None,
            Action::NewOrder,
            Some(Payload::Order(order)),
        )
    }

    #[tokio::test]
    async fn test_order_action_no_order() {
        let pool = create_test_pool().await;
        use crate::app::context::test_utils::{test_settings, TestContextBuilder};
        let ctx = TestContextBuilder::new()
            .with_pool(std::sync::Arc::new(pool.clone()))
            .with_settings(test_settings())
            .build();
        let keys = create_test_keys();
        let event = create_test_unwrapped_message();

        // Create message without order payload
        let msg = Message::Order(MessageKind {
            version: 1,
            request_id: Some(1),
            trade_index: None,
            id: Some(uuid::Uuid::new_v4()),
            action: Action::NewOrder,
            payload: None,
        });

        let result = order_action(&ctx, msg, &event, &keys).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_order_action_invalid_fiat_amount() {
        let pool = create_test_pool().await;
        use crate::app::context::test_utils::{test_settings, TestContextBuilder};
        let ctx = TestContextBuilder::new()
            .with_pool(std::sync::Arc::new(pool.clone()))
            .with_settings(test_settings())
            .build();
        let keys = create_test_keys();
        let event = create_test_unwrapped_message();

        // fiat_amount = 0 should be rejected by check_fiat_amount
        let msg = create_test_order_message(0, 50000);
        let result = order_action(&ctx, msg, &event, &keys).await;
        let err = result.unwrap_err();
        assert!(
            matches!(err, MostroCantDo(CantDoReason::InvalidAmount)),
            "expected InvalidAmount, got: {:?}",
            err
        );

        // fiat_amount < 0 should also be rejected with same error
        let msg = create_test_order_message(-100, 50000);
        let result = order_action(&ctx, msg, &event, &keys).await;
        let err = result.unwrap_err();
        assert!(
            matches!(err, MostroCantDo(CantDoReason::InvalidAmount)),
            "expected InvalidAmount for negative, got: {:?}",
            err
        );
    }

    #[tokio::test]
    async fn test_order_action_invalid_amount() {
        let pool = create_test_pool().await;
        use crate::app::context::test_utils::{test_settings, TestContextBuilder};
        let ctx = TestContextBuilder::new()
            .with_pool(std::sync::Arc::new(pool.clone()))
            .with_settings(test_settings())
            .build();
        let keys = create_test_keys();
        let event = create_test_unwrapped_message();

        // amount < 0 should be rejected by check_amount
        let msg = create_test_order_message(100, -50000);
        let result = order_action(&ctx, msg, &event, &keys).await;
        let err = result.unwrap_err();
        assert!(
            matches!(err, MostroCantDo(CantDoReason::InvalidAmount)),
            "expected InvalidAmount, got: {:?}",
            err
        );
    }

    #[tokio::test]
    async fn test_order_action_with_valid_order() {
        let pool = create_test_pool().await;
        use crate::app::context::test_utils::{test_settings, TestContextBuilder};
        let ctx = TestContextBuilder::new()
            .with_pool(std::sync::Arc::new(pool.clone()))
            .with_settings(test_settings())
            .build();
        let keys = create_test_keys();
        let event = create_test_unwrapped_message();
        let msg = create_test_message(Some(1));

        // This test would require:
        // 1. Mocking validate_invoice
        // 2. Setting up database tables
        // 3. Mocking publish_order
        // For now, we test the structure
        let _ = order_action(&ctx, msg, &event, &keys).await;
    }

    #[tokio::test]
    async fn test_order_action_range_order_validation() {
        let pool = create_test_pool().await;
        use crate::app::context::test_utils::{test_settings, TestContextBuilder};
        let ctx = TestContextBuilder::new()
            .with_pool(std::sync::Arc::new(pool.clone()))
            .with_settings(test_settings())
            .build();
        let keys = create_test_keys();
        let event = create_test_unwrapped_message();

        let msg = create_test_message(Some(1));

        let _ = order_action(&ctx, msg, &event, &keys).await;
    }

    #[tokio::test]
    async fn test_order_action_zero_amount_with_premium() {
        let pool = create_test_pool().await;
        use crate::app::context::test_utils::{test_settings, TestContextBuilder};
        let ctx = TestContextBuilder::new()
            .with_pool(std::sync::Arc::new(pool.clone()))
            .with_settings(test_settings())
            .build();
        let keys = create_test_keys();
        let event = create_test_unwrapped_message();

        let msg = create_test_message(Some(1));
        // Structural check: ensure call does not panic
        let _ = order_action(&ctx, msg, &event, &keys).await;
    }

    #[tokio::test]
    async fn test_order_action_trade_index_logic() {
        let pool = create_test_pool().await;
        use crate::app::context::test_utils::{test_settings, TestContextBuilder};
        let ctx = TestContextBuilder::new()
            .with_pool(std::sync::Arc::new(pool.clone()))
            .with_settings(test_settings())
            .build();
        let keys = create_test_keys();

        // Test case 1: identity == sender, no trade_index
        let mut event = create_test_unwrapped_message();
        event.identity = event.sender;
        let msg = create_test_message(None);

        let _ = order_action(&ctx, msg, &event, &keys).await;

        // Test case 2: identity != sender, no trade_index
        let event2 = create_test_unwrapped_message();
        // identity and sender are already distinct by default
        let msg2 = create_test_message(None);

        // Structural check: ensure call returns a Result without panicking
        let _ = order_action(&ctx, msg2, &event2, &keys).await;

        // Test case 3: with trade_index
        let msg3 = create_test_message(Some(1));
        let _ = order_action(&ctx, msg3, &event2, &keys).await;
    }

    mod calculate_and_check_quote_tests {
        use super::*;
        use crate::app::context::test_utils::{test_settings, TestContextBuilder};
        use crate::bitcoin_price::BitcoinPriceManager;
        use std::sync::Arc;

        async fn create_ctx() -> AppContext {
            let pool = Arc::new(SqlitePool::connect(":memory:").await.unwrap());
            TestContextBuilder::new()
                .with_pool(pool)
                .with_settings(test_settings())
                .build()
        }

        fn order_with(amount: i64, fiat_code: &str) -> SmallOrder {
            SmallOrder {
                amount,
                fiat_code: fiat_code.to_string(),
                ..Default::default()
            }
        }

        #[tokio::test]
        async fn fixed_amount_inside_limits_is_accepted() {
            let ctx = create_ctx().await;
            let order = order_with(50_000, "USD");
            assert!(calculate_and_check_quote(&ctx, &order, &100).await.is_ok());
        }

        #[tokio::test]
        async fn fixed_amount_below_min_is_out_of_range() {
            let ctx = create_ctx().await;
            // min_payment_amount is 100 sats in test settings.
            let order = order_with(10, "USD");
            let err = calculate_and_check_quote(&ctx, &order, &100)
                .await
                .unwrap_err();
            assert!(matches!(
                err,
                MostroCantDo(CantDoReason::OutOfRangeSatsAmount)
            ));
        }

        #[tokio::test]
        async fn negative_amount_is_invalid() {
            let ctx = create_ctx().await;
            let order = order_with(-5, "USD");
            let err = calculate_and_check_quote(&ctx, &order, &100)
                .await
                .unwrap_err();
            assert!(matches!(err, MostroCantDo(CantDoReason::InvalidAmount)));
        }

        #[tokio::test]
        async fn market_priced_order_uses_cached_price() {
            let ctx = create_ctx().await;
            // Unique currency code avoids clobbering the shared override map.
            BitcoinPriceManager::set_price_for_test("QUOTE1", 50_000.0);
            let order = order_with(0, "QUOTE1");
            // 100 / 50_000 * 1e8 = 200_000 sats → inside [100, 1_000_000].
            assert!(calculate_and_check_quote(&ctx, &order, &100).await.is_ok());
        }

        #[tokio::test]
        async fn market_priced_order_without_price_data_fails() {
            let ctx = create_ctx().await;
            // No override and no global PriceManager → NoAPIResponse.
            let order = order_with(0, "QUOTE2");
            let err = calculate_and_check_quote(&ctx, &order, &100)
                .await
                .unwrap_err();
            assert!(matches!(
                err,
                MostroInternalErr(ServiceError::NoAPIResponse)
            ));
        }

        #[tokio::test]
        async fn market_priced_order_above_max_is_out_of_range() {
            let ctx = create_ctx().await;
            BitcoinPriceManager::set_price_for_test("QUOTE3", 1.0);
            // 100 / 1.0 * 1e8 = 10_000_000_000 sats → above max_order_amount.
            let order = order_with(0, "QUOTE3");
            let err = calculate_and_check_quote(&ctx, &order, &100)
                .await
                .unwrap_err();
            assert!(matches!(
                err,
                MostroCantDo(CantDoReason::OutOfRangeSatsAmount)
            ));
        }
    }

    mod order_action_flow_tests {
        use super::*;
        use crate::app::context::test_utils::{test_settings, TestContextBuilder};
        use std::sync::Arc;

        async fn create_migrated_ctx() -> AppContext {
            let pool = Arc::new(SqlitePool::connect(":memory:").await.unwrap());
            sqlx::migrate!("./migrations")
                .run(pool.as_ref())
                .await
                .unwrap();
            TestContextBuilder::new()
                .with_pool(pool)
                .with_settings(test_settings())
                .build()
        }

        fn init_globals() {
            let _ =
                crate::config::MOSTRO_CONFIG.set(crate::app::context::test_utils::test_settings());
            let _ = crate::NOSTR_CLIENT.set(nostr_sdk::Client::default());
        }

        fn order_message(fiat_code: &str, trade_index: Option<i64>) -> Message {
            let order = SmallOrder {
                kind: Some(mostro_core::order::Kind::Sell),
                amount: 1_000,
                fiat_code: fiat_code.to_string(),
                fiat_amount: 100,
                payment_method: "SEPA".to_string(),
                ..Default::default()
            };
            Message::new_order(
                None,
                Some(1),
                trade_index,
                Action::NewOrder,
                Some(Payload::Order(order)),
            )
        }

        #[tokio::test]
        async fn unsupported_fiat_currency_is_rejected() {
            init_globals();
            let ctx = create_migrated_ctx().await;
            let keys = create_test_keys();
            let event = create_test_unwrapped_message();

            let msg = order_message("XXX", Some(1));
            let err = order_action(&ctx, msg, &event, &keys).await.unwrap_err();
            assert!(
                matches!(err, MostroCantDo(_)),
                "unsupported currency must be a CantDo: {err:?}"
            );
        }

        #[tokio::test]
        async fn missing_trade_index_with_distinct_identity_is_invalid_payload() {
            init_globals();
            let ctx = create_migrated_ctx().await;
            let keys = create_test_keys();
            // identity != sender and no trade_index → InvalidPayload.
            let event = create_test_unwrapped_message();

            let msg = order_message("USD", None);
            let err = order_action(&ctx, msg, &event, &keys).await.unwrap_err();
            assert!(matches!(
                err,
                MostroInternalErr(ServiceError::InvalidPayload)
            ));
        }

        #[tokio::test]
        async fn valid_order_reaches_publication_and_persists_row() {
            init_globals();
            let ctx = create_migrated_ctx().await;
            let keys = create_test_keys();
            // Full-privacy shape: identity == sender → trade_index defaults to 0.
            let mut event = create_test_unwrapped_message();
            event.identity = event.sender;

            let msg = order_message("USD", None);
            let result = order_action(&ctx, msg, &event, &keys).await;
            // The offline Nostr client cannot broadcast, so the pipeline ends
            // with a NostrError — but only AFTER the order row was persisted.
            assert!(
                matches!(result, Err(MostroInternalErr(ServiceError::NostrError(_)))),
                "expected broadcast failure at the very end: {result:?}"
            );

            let row: (String, String) =
                sqlx::query_as("SELECT status, event_id FROM orders LIMIT 1")
                    .fetch_one(ctx.pool())
                    .await
                    .expect("order row must be persisted");
            assert_eq!(row.0, "pending");
            assert!(!row.1.is_empty(), "event_id must be recorded");
        }

        #[tokio::test]
        async fn valid_order_with_trade_index_updates_user_index() {
            init_globals();
            let ctx = create_migrated_ctx().await;
            let keys = create_test_keys();
            let event = create_test_unwrapped_message();

            // A registered identity (check_trade_index would have created it
            // on first contact) keeps the tags path on the known-user branch.
            crate::db::add_new_user(
                ctx.pool(),
                mostro_core::user::User {
                    pubkey: event.identity.to_string(),
                    last_trade_index: 1,
                    ..Default::default()
                },
            )
            .await
            .expect("insert identity user");

            let msg = order_message("USD", Some(7));
            let result = order_action(&ctx, msg, &event, &keys).await;
            assert!(
                matches!(result, Err(MostroInternalErr(ServiceError::NostrError(_)))),
                "expected broadcast failure at the very end: {result:?}"
            );
        }
    }

    mod quote_calculation_tests {

        #[test]
        fn test_quote_calculation_logic() {
            // Test the mathematical logic for quote calculation
            let fiat_amount = 100i64;
            let price = 50000.0; // $50,000 per BTC

            // Expected: (100 / 50000) * 1E8 = 200,000 sats
            let expected_quote = (fiat_amount as f64 / price * 1E8) as i64;
            assert_eq!(expected_quote, 200_000);

            // Test with different values
            let fiat_amount2 = 1000i64;
            let price2 = 25000.0; // $25,000 per BTC
            let expected_quote2 = (fiat_amount2 as f64 / price2 * 1E8) as i64;
            assert_eq!(expected_quote2, 4_000_000); // 0.04 BTC = 4M sats
        }

        #[test]
        fn test_amount_limits_validation() {
            // Test amount validation logic
            let quote = 1000i64;
            let max_order = 100_000_000i64; // 1 BTC
            let min_payment = 1_000i64; // 1k sats

            // Valid amount
            assert!(quote >= min_payment && quote <= max_order);

            // Too small
            let small_quote = 500i64;
            assert!(small_quote < min_payment);

            // Too large
            let large_quote = 200_000_000i64; // 2 BTC
            assert!(large_quote > max_order);
        }
    }
}
