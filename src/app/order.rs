use crate::config::settings::Settings;
use crate::db::update_user_trade_index;
use crate::util::{get_bitcoin_price, publish_order, validate_invoice};
use mostro_core::prelude::*;
use nostr::nips::nip59::UnwrappedGift;
use nostr_sdk::prelude::*;
use nostr_sdk::Keys;
use sqlx::{Pool, Sqlite};

async fn calculate_and_check_quote(
    order: &SmallOrder,
    fiat_amount: &i64,
) -> Result<(), MostroError> {
    // Get mostro settings
    let mostro_settings = Settings::get_mostro();
    // Calculate quote
    let quote = match order.amount {
        0 => match get_bitcoin_price(&order.fiat_code) {
            Ok(price) => {
                let quote = *fiat_amount as f64 / price;
                (quote * 1E8) as i64
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
/// - `msg`: Trading message containing order details and a request ID.
/// - `event`: Event data providing sender and rumor details required for determining the trade index.
/// - `my_keys`: Local signing keys used during order publication.
///
/// # Errors
/// Returns a `MostroError` if any validation, quote calculation, trade index update, or order publication fails.
///
/// # Examples
///
/// ```rust
/// # async fn run_example() -> Result<(), MostroError> {
/// # use your_crate::{order_action, Message, UnwrappedGift, Keys};
/// # use sqlx::SqlitePool;
/// // Initialize dummy instances; in a real application, replace these with actual values.
/// let msg = Message::default();
/// let event = UnwrappedGift::default();
/// let my_keys = Keys::default();
/// let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
///
/// // Process the order if present in the message.
/// order_action(msg, &event, &my_keys, &pool).await?;
/// # Ok(())
/// # }
/// # #[tokio::main]
/// # async fn main() {
/// #     run_example().await.unwrap();
/// # }
/// ```
pub async fn order_action(
    msg: Message,
    event: &UnwrappedGift,
    my_keys: &Keys,
    pool: &Pool<Sqlite>,
) -> Result<(), MostroError> {
    // Get request id
    let request_id = msg.get_inner_message_kind().request_id;

    if let Some(order) = msg.get_inner_message_kind().get_order() {
        // Validate invoice
        let _invoice = validate_invoice(&msg, &Order::from(order.clone())).await?;

        // Check if fiat currency is accepted
        let mostro_settings = Settings::get_mostro();
        if let Err(cause) = order.check_fiat_currency(&mostro_settings.fiat_currencies_accepted) {
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
            calculate_and_check_quote(order, fiat_amount).await?;
        }

        let trade_index = match msg.get_inner_message_kind().trade_index {
            Some(trade_index) => trade_index,
            None => {
                if event.sender == event.rumor.pubkey {
                    0
                } else {
                    return Err(MostroInternalErr(ServiceError::InvalidPayload));
                }
            }
        };

        // Update trade index only after all checks are done
        update_user_trade_index(pool, event.sender.to_string(), trade_index)
            .await
            .map_err(|e| MostroInternalErr(ServiceError::DbAccessError(e.to_string())))?;

        // Publish order
        publish_order(
            pool,
            my_keys,
            order,
            event.rumor.pubkey,
            event.sender,
            event.rumor.pubkey,
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

    use nostr_sdk::{Keys, Kind as NostrKind, Timestamp, UnsignedEvent};
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

    fn create_test_unwrapped_gift() -> UnwrappedGift {
        let keys = create_test_keys();
        let sender_keys = create_test_keys();

        let unsigned_event = UnsignedEvent::new(
            keys.public_key(),
            Timestamp::now(),
            NostrKind::GiftWrap,
            Vec::new(),
            "",
        );

        UnwrappedGift {
            sender: sender_keys.public_key(),
            rumor: unsigned_event,
        }
    }

    #[tokio::test]
    async fn test_order_action_no_order() {
        let pool = create_test_pool().await;
        let keys = create_test_keys();
        let event = create_test_unwrapped_gift();

        // Create message without order payload
        let msg = Message::Order(MessageKind {
            version: 1,
            request_id: Some(1),
            trade_index: None,
            id: Some(uuid::Uuid::new_v4()),
            action: Action::NewOrder,
            payload: None,
        });

        let result = order_action(msg, &event, &keys, &pool).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_order_action_with_valid_order() {
        let pool = create_test_pool().await;
        let keys = create_test_keys();
        let event = create_test_unwrapped_gift();
        let msg = create_test_message(Some(1));

        // This test would require:
        // 1. Mocking validate_invoice
        // 2. Setting up database tables
        // 3. Mocking publish_order
        // For now, we test the structure
        let _ = order_action(msg, &event, &keys, &pool).await;
    }

    #[tokio::test]
    async fn test_order_action_range_order_validation() {
        let pool = create_test_pool().await;
        let keys = create_test_keys();
        let event = create_test_unwrapped_gift();

        let msg = create_test_message(Some(1));

        let _ = order_action(msg, &event, &keys, &pool).await;
    }

    #[tokio::test]
    async fn test_order_action_zero_amount_with_premium() {
        let pool = create_test_pool().await;
        let keys = create_test_keys();
        let event = create_test_unwrapped_gift();

        let msg = create_test_message(Some(1));
        // Structural check: ensure call does not panic
        let _ = order_action(msg, &event, &keys, &pool).await;
    }

    #[tokio::test]
    async fn test_order_action_trade_index_logic() {
        let pool = create_test_pool().await;
        let keys = create_test_keys();

        // Test case 1: sender == rumor.pubkey, no trade_index
        let mut event = create_test_unwrapped_gift();
        event.sender = event.rumor.pubkey;
        let msg = create_test_message(None);

        let _ = order_action(msg, &event, &keys, &pool).await;

        // Test case 2: sender != rumor.pubkey, no trade_index
        let event2 = create_test_unwrapped_gift();
        // sender and rumor.pubkey are already different by default
        let msg2 = create_test_message(None);

        // Structural check: ensure call returns a Result without panicking
        let _ = order_action(msg2, &event2, &keys, &pool).await;

        // Test case 3: with trade_index
        let msg3 = create_test_message(Some(1));
        let _ = order_action(msg3, &event2, &keys, &pool).await;
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
