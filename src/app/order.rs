use crate::cli::settings::Settings;
use crate::lightning::invoice::decode_invoice;
use crate::util::{get_bitcoin_price, publish_order, send_cant_do_msg, send_new_order_msg};

use crate::nip59::unwrap_gift_wrap;
use anyhow::Result;
use mostro_core::message::{Action, Message};
use nostr_sdk::{Event, Keys};
use sqlx::{Pool, Sqlite};
use tracing::error;

pub async fn order_action(
    msg: Message,
    event: &Event,
    my_keys: &Keys,
    pool: &Pool<Sqlite>,
) -> Result<()> {
    if let Some(order) = msg.get_inner_message_kind().get_order() {
        let unwrapped_event = unwrap_gift_wrap(my_keys, event)?;
        let mostro_settings = Settings::get_mostro();

        // Reject all new invoices in a neworder with amount != 0
        if let Some(invoice) = msg.get_inner_message_kind().get_payment_request() {
            let invoice = decode_invoice(&invoice)?;
            if invoice
                .amount_milli_satoshis()
                .map(|sats| sats / 1000)
                .is_some()
            {
                send_new_order_msg(
                    None,
                    Action::IncorrectInvoiceAmount,
                    None,
                    &unwrapped_event.sender,
                )
                .await;
                return Ok(());
            }
        }

        // Default case single amount
        let mut amount_vec = vec![order.fiat_amount];

        // Get max and and min amount in case of range order
        // in case of single order do like usual
        if let (Some(min), Some(max)) = (order.min_amount, order.max_amount) {
            if min >= max {
                send_cant_do_msg(order.id, None, &unwrapped_event.sender).await;
                return Ok(());
            }
            if order.amount == 0 {
                amount_vec.clear();
                amount_vec.push(min);
                amount_vec.push(max);
            } else {
                send_new_order_msg(
                    None,
                    Action::InvalidSatsAmount,
                    None,
                    &unwrapped_event.sender,
                )
                .await;
                return Ok(());
            }
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
                send_new_order_msg(
                    None,
                    Action::InvalidSatsAmount,
                    None,
                    &unwrapped_event.sender,
                )
                .await;
                return Ok(());
            }

            if quote > mostro_settings.max_order_amount as i64
                || quote < mostro_settings.min_payment_amount as i64
            {
                send_new_order_msg(
                    None,
                    Action::OutOfRangeSatsAmount,
                    None,
                    &unwrapped_event.sender,
                )
                .await;
                return Ok(());
            }
        }

        let master_pubkey = match msg.get_inner_message_kind().pubkey {
            Some(ref pk) => pk,
            None => {
                // We create a Message
                send_cant_do_msg(order.id, None, &unwrapped_event.sender).await;
                return Ok(());
            }
        };

        publish_order(
            pool,
            my_keys,
            order,
            &unwrapped_event.sender.to_string(),
            master_pubkey,
            unwrapped_event.sender,
        )
        .await?;
    }
    Ok(())
}
