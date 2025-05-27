pub mod app;
mod bitcoin_price;
pub mod cli;
pub mod config;
pub mod db;
pub mod flow;
pub mod lightning;
pub mod lnurl;
pub mod messages;
pub mod models;
pub mod nip33;
pub mod scheduler;
pub mod util;

use crate::app::run;
use crate::cli::settings_init;
use crate::config::{get_db_pool, DB_POOL, LN_STATUS, NOSTR_CLIENT};
use crate::db::find_held_invoices;
use crate::lightning::LnStatus;
use crate::lightning::LndConnector;
use nostr_sdk::prelude::*;
use scheduler::start_scheduler;
use std::env;
use std::process::exit;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};
use util::{get_nostr_client, invoice_subscribe};

#[tokio::main]
async fn main() -> Result<()> {
    if cfg!(debug_assertions) {
        // Debug, show all error + mostro logs
        env::set_var("RUST_LOG", "error,mostro=info");
    } else {
        // Release, show only mostro logs
        env::set_var("RUST_LOG", "none,mostro=info");
    }

    // Tracing using RUST_LOG
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(EnvFilter::from_default_env())
        .init();

    // Init MOSTRO_SETTINGS oncelock with all settings variables from TOML file
    settings_init()?;

    // Connect to database
    if DB_POOL.set(db::connect().await?).is_err() {
        tracing::error!("No connection to database - closing Mostro!");
        exit(1);
    };

    // Connect to relays
    if NOSTR_CLIENT.set(util::connect_nostr().await?).is_err() {
        tracing::error!("No connection to nostr relay - closing Mostro!");
        exit(1);
    };

    let my_keys = util::get_keys()?;
    let subscription = Filter::new()
        .pubkey(my_keys.public_key())
        .kind(Kind::GiftWrap)
        .limit(0);

    let client = match get_nostr_client() {
        Ok(client) => client,
        Err(e) => {
            tracing::error!("Failed to initialize Nostr client. Cannot proceed: {e}");
            // Clean up any resources if needed
            exit(1)
        }
    };

    // Client subscription
    client.subscribe(subscription, None).await?;

    let mut ln_client = LndConnector::new().await?;
    let ln_status = ln_client.get_node_info().await?;
    let ln_status = LnStatus::from_get_info_response(ln_status);
    if LN_STATUS.set(ln_status).is_err() {
        panic!("No connection to LND node - shutting down Mostro!");
    };

    if let Ok(held_invoices) = find_held_invoices(get_db_pool().as_ref()).await {
        for invoice in held_invoices.iter() {
            if let Some(hash) = &invoice.hash {
                tracing::info!("Resubscribing order id - {}", invoice.id);
                if let Err(e) = invoice_subscribe(hash.as_bytes().to_vec(), None).await {
                    tracing::error!("Ln node error {e}")
                }
            }
        }
    }

    // Start scheduler for tasks
    start_scheduler().await;

    // Run the Mostro and be happy!!
    run(my_keys, client, &mut ln_client).await
}

#[cfg(test)]
mod tests {
    use mostro_core::message::Message;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn test_message_deserialize_serialize() {
        let sample_message = r#"{"order":{"version":1,"request_id":1,"trade_index":null,"id":"7dd204d2-d06c-4406-a3d9-4415f4a8b9c9","action":"fiat-sent","payload":null}}"#;
        let message = Message::from_json(sample_message).unwrap();
        assert!(message.verify());
        let json_message = message.as_json().unwrap();
        assert_eq!(sample_message, json_message);
    }

    #[test]
    fn test_wrong_message_should_fail() {
        let sample_message = r#"{"order":{"version":1,"request_id":1,"action":"take-sell","payload":{"order":{"kind":"sell","status":"pending","amount":100,"fiat_code":"XXX","fiat_amount":10,"payment_method":"SEPA","premium":1,"payment_request":null,"created_at":1640839235}}}}"#;
        let message = Message::from_json(sample_message).unwrap();
        assert!(!message.verify());
    }

    #[test]
    fn test_fee_rounding() {
        let fee = 0.003 / 2.0;

        let mut amt = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .subsec_micros() as i64;

        // Test 1000 "random" amounts
        for _i in 1..=1000 {
            let fee_calculated = fee * amt as f64;
            let rounded_fee = fee_calculated.round();
            // Seller side
            let seller_total_amt = rounded_fee as i64 + amt;
            assert_eq!(amt, seller_total_amt - rounded_fee as i64);
            // Buyer side

            let buyer_total_amt = amt - rounded_fee as i64;
            assert_eq!(amt, buyer_total_amt + rounded_fee as i64);

            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .subsec_millis() as i64;

            amt %= 100_000_i64;
            amt *= (rounded_fee as i64) % 100_i64;
            amt += nonce;
        }
    }
}
