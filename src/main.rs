pub mod app;
pub mod db;
pub mod error;
pub mod flow;
pub mod lightning;
pub mod messages;
pub mod models;
pub mod scheduler;
pub mod util;

use crate::app::run;
use crate::util::check_env_vars;
use anyhow::Result;
use dotenvy::dotenv;
use lightning::LndConnector;
use nostr_sdk::prelude::*;
use scheduler::start_scheduler;
use tokio::sync::Mutex;

#[macro_use]
extern crate lazy_static;

lazy_static! {
    static ref RATE_EVENT_LIST: Mutex<Vec<Event>> = Mutex::new(vec![]);
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenv().ok();
    check_env_vars()?;
    pretty_env_logger::init();
    // Connect to database
    let pool = db::connect().await?;
    // Connect to relays
    let client = util::connect_nostr().await?;
    let my_keys = util::get_keys()?;

    println!("pub {}", my_keys.public_key().to_bech32().unwrap());

    let subscription = Filter::new()
        .pubkey(my_keys.public_key())
        .since(Timestamp::now());

    client.subscribe(vec![subscription]).await;
    let mut ln_client = LndConnector::new().await;

    // Start scheduler for tasks
    start_scheduler().await.unwrap().start().await?;

    run(my_keys, client, &mut ln_client, pool).await
}

#[cfg(test)]
mod tests {
    use dotenvy::dotenv;
    use mostro_core::order::NewOrder;
    use mostro_core::Message;
    use std::env::var;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn test_order_deserialize_serialize() {
        let sample_order = r#"{"kind":"Sell","status":"Pending","amount":100,"fiat_code":"XXX","fiat_amount":10,"payment_method":"belo","premium":1}"#;
        let order = NewOrder::from_json(sample_order).unwrap();
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
        dotenv().ok();
        let fee = var("FEE")
            .unwrap_or(0.003.to_string())
            .parse::<f64>()
            .unwrap_or(0.003)
            / 2.0;

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
