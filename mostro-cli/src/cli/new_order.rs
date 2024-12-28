use anyhow::Result;
use mostro_core::message::{Action, Message, Payload};
use mostro_core::order::SmallOrder;
use mostro_core::order::{Kind, Status};
use nostr_sdk::prelude::*;
use std::collections::HashMap;
use std::io::{stdin, stdout, BufRead, Write};
use std::process;
use std::str::FromStr;
use uuid::Uuid;

use crate::db::{connect, Order, User};
use crate::pretty_table::print_order_preview;
use crate::util::{send_message_sync, uppercase_first};

pub type FiatNames = HashMap<String, String>;

#[allow(clippy::too_many_arguments)]
pub async fn execute_new_order(
    kind: &str,
    fiat_code: &str,
    fiat_amount: &(i64, Option<i64>),
    amount: &i64,
    payment_method: &String,
    premium: &i64,
    invoice: &Option<String>,
    identity_keys: &Keys,
    trade_keys: &Keys,
    trade_index: i64,
    mostro_key: PublicKey,
    client: &Client,
    expiration_days: &i64,
) -> Result<()> {
    // Uppercase currency
    let fiat_code = fiat_code.to_uppercase();
    // Check if fiat currency selected is available on Yadio and eventually force user to set amount
    // this is in the case of crypto <--> crypto offer for example
    if *amount == 0 {
        // Get Fiat list
        let api_req_string = "https://api.yadio.io/currencies".to_string();
        let fiat_list_check = reqwest::get(api_req_string)
            .await?
            .json::<FiatNames>()
            .await?
            .contains_key(&fiat_code);
        if !fiat_list_check {
            println!("{} is not present in the fiat market, please specify an amount with -a flag to fix the rate", fiat_code);
            process::exit(0);
        }
    }
    let kind = uppercase_first(kind);
    // New check against strings
    let kind_checked = Kind::from_str(&kind).unwrap();
    let expires_at = match *expiration_days {
        0 => None,
        _ => {
            let now = chrono::Utc::now();
            let expires_at = now + chrono::Duration::days(*expiration_days);
            Some(expires_at.timestamp())
        }
    };

    // Get the type of neworder
    // if both tuple field are valid than it's a range order
    // otherwise use just fiat amount value as before
    let amt = if fiat_amount.1.is_some() {
        (0, Some(fiat_amount.0), fiat_amount.1)
    } else {
        (fiat_amount.0, None, None)
    };
    let small_order = SmallOrder::new(
        None,
        Some(kind_checked),
        Some(Status::Pending),
        *amount,
        fiat_code.clone(),
        amt.1,
        amt.2,
        amt.0,
        payment_method.to_owned(),
        *premium,
        None,
        None,
        invoice.as_ref().to_owned().cloned(),
        Some(0),
        expires_at,
        None,
        None,
    );

    // Create new order for mostro
    let order_content = Payload::Order(small_order.clone());

    // Print order preview
    let ord_preview = print_order_preview(order_content.clone()).unwrap();
    println!("{ord_preview}");
    let mut user_input = String::new();
    let _input = stdin();
    print!("Check your order! Is it correct? (Y/n) > ");
    stdout().flush()?;

    let mut answer = stdin().lock();
    answer.read_line(&mut user_input)?;

    match user_input.to_lowercase().as_str().trim_end() {
        "y" | "" => {}
        "n" => {
            println!("Ok you have cancelled the order, create another one please");
            process::exit(0);
        }
        &_ => {
            println!("Can't get what you're sayin!");
            process::exit(0);
        }
    };
    let request_id = Uuid::new_v4().as_u128() as u64;
    // Create NewOrder message
    let message = Message::new_order(
        None,
        Some(request_id),
        Some(trade_index),
        Action::NewOrder,
        Some(order_content),
    );

    let dm = send_message_sync(
        client,
        Some(identity_keys),
        trade_keys,
        mostro_key,
        message,
        true,
        false,
    )
    .await?;
    let order_id = dm
        .iter()
        .find_map(|el| {
            let message = el.0.get_inner_message_kind();
            if message.request_id == Some(request_id) {
                match message.action {
                    Action::NewOrder => {
                        if let Some(Payload::Order(order)) = message.payload.as_ref() {
                            return order.id;
                        }
                    }
                    Action::OutOfRangeFiatAmount | Action::OutOfRangeSatsAmount => {
                        println!("Error: Amount is outside the allowed range. Please check the order's min/max limits.");
                        return None;
                    }
                    _ => {
                        println!("Unknown action: {:?}", message.action);
                        return None;
                    }
                }
            }
            None
        })
        .or_else(|| {
            println!("Error: No matching order found in response");
            None
        });

    if let Some(order_id) = order_id {
        println!("Order id {} created", order_id);
        // Create order in db
        let pool = connect().await?;
        let db_order = Order::new(&pool, small_order, trade_keys, Some(request_id as i64))
            .await
            .map_err(|e| anyhow::anyhow!("Failed to create DB order: {:?}", e))?;
        // Update last trade index
        match User::get(&pool).await {
            Ok(mut user) => {
                user.set_last_trade_index(trade_index + 1);
                if let Err(e) = user.save(&pool).await {
                    println!("Failed to update user: {}", e);
                }
            }
            Err(e) => println!("Failed to get user: {}", e),
        }
        let db_order_id = db_order
            .id
            .clone()
            .ok_or(anyhow::anyhow!("Missing order id"))?;
        Order::save_new_id(&pool, db_order_id, order_id.to_string()).await?;
    }
    Ok(())
}
