use anyhow::Result;
use lnurl::lightning_address::LightningAddress;
use mostro_core::message::{Action, Message, Payload};
use nostr_sdk::prelude::*;
use std::str::FromStr;
use uuid::Uuid;

use crate::db::{connect, Order, User};
use crate::lightning::is_valid_invoice;
use crate::util::send_message_sync;

#[allow(clippy::too_many_arguments)]
pub async fn execute_take_sell(
    order_id: &Uuid,
    invoice: &Option<String>,
    amount: Option<u32>,
    identity_keys: &Keys,
    trade_keys: &Keys,
    trade_index: i64,
    mostro_key: PublicKey,
    client: &Client,
) -> Result<()> {
    println!(
        "Request of take sell order {} from mostro pubId {}",
        order_id,
        mostro_key.clone()
    );
    let mut payload = None;
    if let Some(invoice) = invoice {
        // Check invoice string
        let ln_addr = LightningAddress::from_str(invoice);
        if ln_addr.is_ok() {
            payload = Some(Payload::PaymentRequest(None, invoice.to_string(), None));
        } else {
            match is_valid_invoice(invoice) {
                Ok(i) => payload = Some(Payload::PaymentRequest(None, i.to_string(), None)),
                Err(e) => println!("{}", e),
            }
        }
    }

    // Add amount in case it's specified
    if amount.is_some() {
        payload = match payload {
            Some(Payload::PaymentRequest(a, b, _)) => {
                Some(Payload::PaymentRequest(a, b, Some(amount.unwrap() as i64)))
            }
            None => Some(Payload::Amount(amount.unwrap().into())),
            _ => None,
        };
    }
    let request_id = Uuid::new_v4().as_u128() as u64;
    // Create takesell message
    let take_sell_message = Message::new_order(
        Some(*order_id),
        Some(request_id),
        Some(trade_index),
        Action::TakeSell,
        payload,
    );

    let dm = send_message_sync(
        client,
        Some(identity_keys),
        trade_keys,
        mostro_key,
        take_sell_message,
        true,
        false,
    )
    .await?;
    let pool = connect().await?;

    let order = dm.iter().find_map(|el| {
        let message = el.0.get_inner_message_kind();
        if message.request_id == Some(request_id) {
            match message.action {
                Action::AddInvoice => {
                    if let Some(Payload::Order(order)) = message.payload.as_ref() {
                        println!(
                            "Please add a lightning invoice with amount of {}",
                            order.amount
                        );
                        return Some(order.clone());
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
    });
    if let Some(o) = order {
        match Order::new(&pool, o, trade_keys, Some(request_id as i64)).await {
            Ok(order) => {
                if let Some(order_id) = order.id {
                    println!("Order {} created", order_id);
                } else {
                    println!("Warning: The newly created order has no ID.");
                }
                // Update last trade index to be used in next trade
                match User::get(&pool).await {
                    Ok(mut user) => {
                        user.set_last_trade_index(trade_index + 1);
                        if let Err(e) = user.save(&pool).await {
                            println!("Failed to update user: {}", e);
                        }
                    }
                    Err(e) => println!("Failed to get user: {}", e),
                }
            }
            Err(e) => println!("{}", e),
        }
    }

    Ok(())
}
