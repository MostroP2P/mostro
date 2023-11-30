pub mod app;
pub mod cli;
pub mod db;
pub mod error;
pub mod flow;
pub mod lightning;
pub mod messages;
pub mod models;
pub mod nip33;
pub mod scheduler;
pub mod util;

use crate::app::run;
use crate::cli::settings::{init_global_settings, Settings};
use crate::cli::settings_init;
use anyhow::Result;
use lightning::LndConnector;
use nostr_sdk::prelude::*;
use scheduler::start_scheduler;
use std::env;
use std::sync::Arc;
use std::sync::OnceLock;
use tokio::sync::Mutex;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

static MOSTRO_CONFIG: OnceLock<Settings> = OnceLock::new();

#[tokio::main]
async fn main() -> Result<()> {
    env::set_var("RUST_LOG", "none,mostro=info");

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
    // // Connect to relays
    let client = util::connect_nostr().await?;
    let my_keys = util::get_keys()?;

    let subscription = Filter::new()
        .pubkey(my_keys.public_key())
        .since(Timestamp::now());

    client.subscribe(vec![subscription]).await;
    let mut ln_client = LndConnector::new().await;

    // Start scheduler for tasks
    start_scheduler(rate_list.clone(), &client).await;

    run(my_keys, client, &mut ln_client, pool, rate_list.clone()).await
}

#[cfg(test)]
mod tests {
    use mostro_core::message::Message;
    use mostro_core::order::SmallOrder;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn test_order_deserialize_serialize() {
        let sample_order = r#"{"kind":"Sell","status":"Pending","amount":100,"fiat_code":"XXX","fiat_amount":10,"payment_method":"belo","premium":1,"created_at":0}"#;
        let order = SmallOrder::from_json(sample_order).unwrap();
        let json_order = order.as_json().unwrap();
        assert_eq!(sample_order, json_order);
    }

    #[test]
    fn test_message_deserialize_serialize() {
        let sample_message = r#"{"version":0,"order_id":"7dd204d2-d06c-4406-a3d9-4415f4a8b9c9","pubkey":null,"action":"TakeSell","content":{"PaymentRequest":[null,"lnbc1..."]}}"#;
        let message = Message::from_json(sample_message).unwrap();
        assert!(message.verify());
        let json_message = message.as_json().unwrap();
        assert_eq!(sample_message, json_message);
    }

    #[test]
    fn test_wrong_message_should_fail() {
        let sample_message = r#"{"version":0,"action":"TakeSell","content":{"Order":{"kind":"Sell","status":"Pending","amount":100,"fiat_code":"XXX","fiat_amount":10,"payment_method":"belo","premium":1,"payment_request":null,"created_at":1640839235}}}"#;
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
