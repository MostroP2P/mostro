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
use anyhow::{Context, Result};
use dotenvy::dotenv;
use lightning::LndConnector;
use nostr_sdk::prelude::*;
use scheduler::start_scheduler;
use std::env::var;
use tokio::sync::Mutex;

#[macro_use]
extern crate lazy_static;

lazy_static! {
    static ref RATE_EVENT_LIST: Mutex<Vec<Event>> = Mutex::new(vec![]);
}

pub fn check_env_vars() -> Result<()> {
    // Mandatory env vars
    let _ = var("NSEC_PRIVKEY")
        .context("Missing NSEC_PRIVKEY env variable from env file - add mostro private key")?;
    let _ = var("RELAYS")
        .context("Missing RELAYS env variable from env file - add relay list comma separated")?;
    let _ = var("DATABASE_URL").context("Missing DATABASE_URL from env file - Add a path")?;
    let _ = var("LND_CERT_FILE").context("Missing LND_CERT_FILE from env file - Add a path")?;
    let _ =
        var("LND_MACAROON_FILE").context("Missing LND_MACAROON_FILE from env file - Add a path")?;
    let _ = var("LND_GRPC_PORT")
        .context("Missing LND_GRPC_PORT from env file - Add port value")?
        .parse::<u64>()
        .context("Error parsing LND_GRPC_PORT")?;
    let _ = var("LND_GRPC_HOST").context("Missing LND_GRPC_HOST from env file - set host value")?;
    let _ = var("INVOICE_EXPIRATION_WINDOW")
        .context("Missing INVOICE_EXPIRATION_WINDOW from env file - Add expiration value")?
        .parse::<u64>()
        .context("Error parsing INVOICE_EXPIRATION_WINDOW")?;
    let _ = var("HOLD_INVOICE_CLTV_DELTA")
        .context("Missing HOLD_INVOICE_CLTV_DELTA from env file - Add cltv invoice value")?
        .parse::<u64>()
        .context("Error parsing HOLD_INVOICE_CLTV_DELTA")?;
    let _ = var("MIN_PAYMENT_AMT")
        .context("Missing MIN_PAYMENT_AMT from env file - Add min payment value")?
        .parse::<u64>()
        .context("Error parsing MIN_PAYMENT_AMT")?;
    let _ = var("EXP_SECONDS")
        .context("Missing EXP_SECONDS from env file - Add expiration invoice seconds value")?
        .parse::<u64>()
        .context("Error parsing EXP_SECONDS")?;
    let _ = var("EXP_HOURS")
        .context("Missing EXP_HOURS from env file - Add expiration order hours value")?
        .parse::<u64>()
        .context("Error parsing EXP_HOURS")?;
    let _ = var("MAX_ROUTING_FEE")
        .context("Missing MAX_ROUTING_FEE from env file - Add routing fees value")?
        .parse::<f64>()
        .context("Error parsing MAX_ROUTING_FEE")?;
    let _ = var("FEE")
        .context("Missing FEE from env file - Add mostro fees value")?
        .parse::<f64>()
        .context("Error parsing FEE")?;

    Ok(())
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
    use mostro_core::order::NewOrder;
    use mostro_core::Message;

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
}
