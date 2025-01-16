use crate::cli::settings::Settings;
use crate::util::{get_bitcoin_price, publish_order, validate_invoice};
use anyhow::Result;
use mostro_core::error::{CantDoReason, MostroCantDo, MostroError};
use mostro_core::message::Message;
use mostro_core::order::Order;
use nostr::nips::nip59::UnwrappedGift;
use nostr_sdk::prelude::*;
use nostr_sdk::Keys;
use sqlx::{Pool, Sqlite};
use tracing::error;

pub async fn order_action(
    msg: Message,
    event: &UnwrappedGift,
    my_keys: &Keys,
    pool: &Pool<Sqlite>,
) -> Result<(), MostroError> {
    // Get request id
    let request_id = msg.get_inner_message_kind().request_id;

    if let Some(order) = msg.get_inner_message_kind().get_order() {
        let mostro_settings = Settings::get_mostro();

        // Validate invoice
        let invoice = validate_invoice(&msg, &Order::from(*order)).await?;

        // Default case single amount
        let mut amount_vec = vec![order.fiat_amount];
        // Get max and and min amount in case of range order
        // in case of single order do like usual
        if let (Some(min), Some(max)) = (order.min_amount, order.max_amount) {
            if min >= max {
                return Err(MostroCantDo(CantDoReason::InvalidAmount));
            }
            if order.amount != 0 {
                return Err(MostroCantDo(CantDoReason::InvalidAmount));
            }
            amount_vec.clear();
            amount_vec.push(min);
            amount_vec.push(max);
        }

        let premium = (order.premium != 0).then_some(order.premium);
        let fiat_amount = (order.fiat_amount != 0).then_some(order.fiat_amount);

        if premium.is_some() && fiat_amount.is_some() {
            return Err(MostroCantDo(CantDoReason::InvalidParameters));
        }

        for fiat_amount in amount_vec.iter() {
            let quote = match order.amount {
                0 => match get_bitcoin_price(&order.fiat_code) {
                    Ok(price) => {
                        let quote = *fiat_amount as f64 / price;
                        (quote * 1E8) as i64
                    }
                    Err(e) => {
                        error!("{:?}", e.to_string());
                        return Ok(());
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
        }

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
        .await?;
    }
    Ok(())
}
