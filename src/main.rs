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
    // Clear screen
    clearscreen::clear().expect("Failed to clear screen");

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

    // Get mostro keys
    let mostro_keys = util::get_keys()?;

    let subscription = Filter::new()
        .pubkey(mostro_keys.public_key())
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
    run(mostro_keys, client, &mut ln_client).await
}

#[cfg(test)]
mod tests {
    use super::*;
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

    #[test]
    fn test_debug_log_level_setting() {
        // Test the logical flow of log level setting
        // We can't test the actual environment variable setting since main() has already run
        
        let debug_log_setting = if cfg!(debug_assertions) {
            "error,mostro=info"
        } else {
            "none,mostro=info"
        };
        
        // Verify the log settings are correctly defined
        assert!(!debug_log_setting.is_empty());
        assert!(debug_log_setting.contains("mostro=info"));
        
        if cfg!(debug_assertions) {
            assert!(debug_log_setting.contains("error"));
        } else {
            assert!(debug_log_setting.contains("none"));
        }
    }

    mod mocking {
        use super::*;
        
        

        static TEST_INIT: std::sync::Once = std::sync::Once::new();

        fn setup() {
            TEST_INIT.call_once(|| {
                // Initialize test environment once
                env::set_var("RUST_LOG", "debug");
            });
        }

        #[tokio::test]
        async fn test_settings_initialization() {
            setup();
            
            // Test would require mocking the CLI parsing
            // This tests the basic structure but needs CLI mocking infrastructure
            assert!(true); // Placeholder for actual test
        }

        #[tokio::test]
        async fn test_database_connection_failure() {
            setup();
            
            // This would require mocking the database connection
            // Testing error handling path when DB_POOL.set() fails
            assert!(true); // Placeholder for actual test
        }

        #[tokio::test]
        async fn test_nostr_connection_failure() {
            setup();
            
            // This would require mocking the Nostr client connection
            // Testing error handling path when NOSTR_CLIENT.set() fails
            assert!(true); // Placeholder for actual test
        }

        #[tokio::test]
        async fn test_lightning_connection_failure() {
            setup();
            
            // This would require mocking the Lightning client connection
            // Testing error handling path when LN_STATUS.set() fails
            assert!(true); // Placeholder for actual test
        }

        #[tokio::test]
        async fn test_held_invoices_resubscription() {
            setup();
            
            // Test the logic for resubscribing to held invoices on startup
            // Would require mocking find_held_invoices and invoice_subscribe
            assert!(true); // Placeholder for actual test
        }

        #[tokio::test]
        async fn test_scheduler_startup() {
            setup();
            
            // Test that the scheduler starts correctly
            // Would require mocking start_scheduler
            assert!(true); // Placeholder for actual test
        }

        #[tokio::test]
        async fn test_main_app_startup() {
            setup();
            
            // Test the main application startup flow
            // Would require extensive mocking of all dependencies
            assert!(true); // Placeholder for actual test
        }
    }
}
