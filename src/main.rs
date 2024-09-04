pub mod app;
mod bitcoin_price;
pub mod cli;
pub mod db;
pub mod error;
pub mod flow;
pub mod lightning;
pub mod lnurl;
pub mod messages;
pub mod models;
pub mod nip33;
pub mod scheduler;
pub mod util;

use crate::app::run;
use crate::cli::settings::{init_global_settings, Settings};
use crate::cli::settings_init;
use anyhow::Result;
use db::find_held_invoices;
use lightning::LndConnector;
use nostr_sdk::prelude::*;
use scheduler::start_scheduler;
use std::env;
use std::sync::Arc;
use std::sync::OnceLock;
use tokio::sync::Mutex;
use tracing::{error, info};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};
use util::invoice_subscribe;

static MOSTRO_CONFIG: OnceLock<Settings> = OnceLock::new();
static NOSTR_CLIENT: OnceLock<Client> = OnceLock::new();

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

    let rate_list: Arc<Mutex<Vec<Event>>> = Arc::new(Mutex::new(vec![]));

    // Init path from cli
    let config_path = settings_init()?;

    // Create config global var
    init_global_settings(Settings::new(config_path)?);

    // Connect to database
    let pool = db::connect().await?;

    // Connect to relays
    // from now unwrap is safe - oncelock inited
    if NOSTR_CLIENT.set(util::connect_nostr().await?).is_err() {
        error!("No connection to nostr relay - closing Mostro!");
    };

    let my_keys = util::get_keys()?;

    let subscription = Filter::new()
        .pubkey(my_keys.public_key())
        .since(Timestamp::now());

    NOSTR_CLIENT
        .get()
        .unwrap()
        .subscribe(vec![subscription], None)
        .await;
    let mut ln_client = LndConnector::new().await?;

    if let Ok(held_invoices) = find_held_invoices(&pool).await {
        for invoice in held_invoices.iter() {
            if let Some(hash) = &invoice.hash {
                info!("Resubscribing order id - {}", invoice.id);
                let _ = invoice_subscribe(hash.as_bytes().to_vec()).await;
            }
        }
    }

    // Start scheduler for tasks
    start_scheduler(rate_list.clone()).await;

    run(
        my_keys,
        NOSTR_CLIENT.get().unwrap(),
        &mut ln_client,
        pool,
        rate_list.clone(),
    )
    .await
}

#[cfg(test)]
mod tests {
    use mostro_core::message::Message;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn test_message_deserialize_serialize() {
        let sample_message = r#"{"order":{"version":1,"id":"7dd204d2-d06c-4406-a3d9-4415f4a8b9c9","pubkey":null,"action":"fiat-sent","content":null}}"#;
        let message = Message::from_json(sample_message).unwrap();
        assert!(message.verify());
        let json_message = message.as_json().unwrap();
        assert_eq!(sample_message, json_message);
    }

    #[test]
    fn test_wrong_message_should_fail() {
        let sample_message = r#"{"order":{"version":1,"pubkey":null,"action":"take-sell","content":{"order":{"kind":"sell","status":"pending","amount":100,"fiat_code":"XXX","fiat_amount":10,"payment_method":"SEPA","premium":1,"payment_request":null,"created_at":1640839235}}}}"#;
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
