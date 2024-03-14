use crate::cli::settings::Settings;
use crate::lightning::invoice::is_valid_invoice;
use crate::util::{get_market_quote, publish_order, send_dm};

use anyhow::Result;
use mostro_core::message::{Content, Message};
use nostr_sdk::{Client, Event, Keys};
use sqlx::{Pool, Sqlite};
use tracing::error;

pub async fn order_action(
    msg: Message,
    event: &Event,
    my_keys: &Keys,
    client: &Client,
    pool: &Pool<Sqlite>,
) -> Result<()> {
    if let Some(order) = msg.get_inner_message_kind().get_order() {
        let mostro_settings = Settings::get_mostro();

        // We get the invoice inside the order if present and we check if it's correct:
        // -- Ln address invoice with amount 0 is ok
        // -- Bolt invoice with amount 0 must have also order amount to 0
        if let Some(pay) = msg.get_inner_message_kind().get_payment_request() {
            is_valid_invoice(pay, Some(order.amount as u64), None).await?;
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
            let message = Message::cant_do(
                order.id,
                None,
                Some(Content::TextMessage(format!(
                    "Amount must be positive {} is not valid",
                    order.amount
                ))),
            );
            let message = message.as_json()?;
            send_dm(client, my_keys, &event.pubkey, message).await?;

            return Ok(());
        }

        if quote > mostro_settings.max_order_amount as i64 {
            let message = Message::cant_do(
                order.id,
                None,
                Some(Content::TextMessage(format!(
                    "Quote too high, max is {}",
                    mostro_settings.max_order_amount
                ))),
            );
            let message = message.as_json()?;
            send_dm(client, my_keys, &event.pubkey, message).await?;

            return Ok(());
        }

        let initiator_ephemeral_pubkey = event.pubkey.to_string();
        let master_pubkey = match msg.get_inner_message_kind().pubkey {
            Some(ref pk) => pk,
            None => {
                // We create a Message
                let message = Message::cant_do(order.id, None, None);
                let message = message.as_json()?;
                send_dm(client, my_keys, &event.pubkey, message).await?;

                return Ok(());
            }
        };

        publish_order(
            pool,
            client,
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
