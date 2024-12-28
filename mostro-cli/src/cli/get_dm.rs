use std::collections::HashSet;

use anyhow::Result;
use chrono::DateTime;
use mostro_core::message::{Message, Payload};
use nostr_sdk::prelude::*;

use crate::{
    db::{connect, Order},
    util::get_direct_messages,
};

pub async fn execute_get_dm(
    since: &i64,
    trade_keys: Keys,
    client: &Client,
    from_user: bool,
) -> Result<()> {
    let mut dm: Vec<(Message, u64)> = Vec::new();
    let pool = connect().await?;
    let orders = Order::get_all(&pool).await.unwrap();
    let trade_keys_hex = trade_keys.secret_key().to_secret_hex();
    let order_trade_keys = orders
        .iter()
        .filter_map(|order| order.trade_keys.as_ref().cloned())
        .collect::<Vec<String>>();
    let mut unique_trade_keys = order_trade_keys
        .iter()
        .cloned()
        .collect::<HashSet<String>>();
    unique_trade_keys.insert(trade_keys_hex);
    let final_trade_keys = unique_trade_keys.iter().cloned().collect::<Vec<String>>();
    for keys in final_trade_keys.iter() {
        let trade_keys =
            Keys::parse(keys).map_err(|e| anyhow::anyhow!("Failed to parse trade keys: {}", e))?;
        let dm_temp = get_direct_messages(client, &trade_keys, *since, from_user).await;
        dm.extend(dm_temp);
    }

    if dm.is_empty() {
        println!();
        println!("No new messages");
        println!();
    } else {
        for m in dm.iter() {
            let date = DateTime::from_timestamp(m.1 as i64, 0).unwrap();
            if m.0.get_inner_message_kind().id.is_some() {
                println!(
                    "Mostro sent you this message for order id: {} at {}",
                    m.0.get_inner_message_kind().id.unwrap(),
                    date
                );
            }
            if let Some(Payload::PaymentRequest(_, inv, _)) = &m.0.get_inner_message_kind().payload
            {
                println!();
                println!("Pay this invoice to continue --> {}", inv);
                println!();
            } else if let Some(Payload::TextMessage(text)) = &m.0.get_inner_message_kind().payload {
                println!();
                println!("{text}");
                println!();
            } else {
                println!();
                println!("Action: {}", m.0.get_inner_message_kind().action);
                println!("Payload: {:#?}", m.0.get_inner_message_kind().payload);
                println!();
            }
        }
    }
    Ok(())
}
