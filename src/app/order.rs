use crate::cli::settings::Settings;
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
