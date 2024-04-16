use crate::cli::settings::Settings;
use crate::lightning::invoice::decode_invoice;
use crate::util::{get_market_quote, publish_order, send_cant_do_msg};

use anyhow::Result;
use mostro_core::message::Message;
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
        let mostro_settings = Settings::get_mostro();

        // Reject all new invoices in a neworder with amount != 0
        if let Some(invoice) = msg.get_inner_message_kind().get_payment_request() {
            let invoice = decode_invoice(&invoice)?;
            if invoice
                .amount_milli_satoshis()
                .map(|sats| sats / 1000)
                .is_some()
            {
                let error = String::from("Invoice with an amount different from zero receive on new order, please send 0 amount invoice or no invoice at all!");
                send_cant_do_msg(order.id, Some(error), &event.pubkey).await;

                return Ok(());
            }
        }

        let quote = match order.amount {
            0 => match get_market_quote(&order.fiat_amount, &order.fiat_code, 0).await {
                Ok(amount) => amount,
                Err(e) => {
                    error!("{:?}", e.to_string());
                    return Ok(());
                }
            },
            _ => order.amount,
        };

        // Check amount is positive - extra safety check
        if quote < 0 {
            let msg = format!("Amount must be positive {} is not valid", order.amount);
            send_cant_do_msg(order.id, Some(msg), &event.pubkey).await;
            return Ok(());
        }

        if quote > mostro_settings.max_order_amount as i64
            || quote < mostro_settings.min_payment_amount as i64
        {
            let msg = format!(
                "Quote is out of sats boundaries min is {} max is {}",
                mostro_settings.min_payment_amount, mostro_settings.max_order_amount
            );
            send_cant_do_msg(order.id, Some(msg), &event.pubkey).await;
            return Ok(());
        }

        let initiator_ephemeral_pubkey = event.pubkey.to_string();
        let master_pubkey = match msg.get_inner_message_kind().pubkey {
            Some(ref pk) => pk,
            None => {
                // We create a Message
                send_cant_do_msg(order.id, None, &event.pubkey).await;
                return Ok(());
            }
        };

        publish_order(
            pool,
            my_keys,
            order,
            &initiator_ephemeral_pubkey,
            master_pubkey,
            event.pubkey,
        )
        .await?;
    }
    Ok(())
}
