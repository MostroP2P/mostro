use crate::cli::settings::Settings;
use crate::db::update_user_trade_index;
use crate::util::{get_bitcoin_price, publish_order, validate_invoice};
use mostro_core::error::{
    CantDoReason,
    MostroError::{self, *},
    ServiceError,
};
use mostro_core::message::Message;
use mostro_core::order::{Order, SmallOrder};
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

        // Update trade index only after all checks are done
        update_user_trade_index(
            pool,
            event.sender.to_string(),
            msg.get_inner_message_kind().trade_index.unwrap(),
        )
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
        .await
        .map_err(|_| MostroError::MostroInternalErr(ServiceError::InvalidOrderId))?;
    }
    Ok(())
}
