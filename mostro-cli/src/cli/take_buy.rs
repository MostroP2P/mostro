use anyhow::Result;
use mostro_core::message::{Action, Message, Payload};
use nostr_sdk::prelude::*;
use uuid::Uuid;

use crate::{
    db::{connect, Order, User},
    util::send_message_sync,
};

pub async fn execute_take_buy(
    order_id: &Uuid,
    amount: Option<u32>,
    identity_keys: &Keys,
    trade_keys: &Keys,
    trade_index: i64,
    mostro_key: PublicKey,
    client: &Client,
) -> Result<()> {
    println!(
        "Request of take buy order {} from mostro pubId {}",
        order_id,
        mostro_key.clone()
    );
    let request_id = Uuid::new_v4().as_u128() as u64;
    let payload = amount.map(|amt: u32| Payload::Amount(amt as i64));
    // Create takebuy message
    let take_buy_message = Message::new_order(
        Some(*order_id),
        Some(request_id),
        Some(trade_index),
        Action::TakeBuy,
        payload,
    );

    let dm = send_message_sync(
        client,
        Some(identity_keys),
        trade_keys,
        mostro_key,
        take_buy_message,
        true,
        false,
    )
    .await?;

    let pool = connect().await?;

    let order = dm.iter().find_map(|el| {
        let message = el.0.get_inner_message_kind();
        if message.request_id == Some(request_id) {
            match message.action {
                Action::PayInvoice => {
                    if let Some(Payload::PaymentRequest(order, invoice, _)) = &message.payload {
                        println!(
                            "Mostro sent you this hold invoice for order id: {}",
                            order
                                .as_ref()
                                .and_then(|o| o.id)
                                .map_or("unknown".to_string(), |id| id.to_string())
                        );
                        println!();
                        println!("Pay this invoice to continue -->  {}", invoice);
                        println!();
                        return order.clone();
                    }
                }
                Action::OutOfRangeFiatAmount | Action::OutOfRangeSatsAmount=> {
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
                println!("Order {} created", order.id.unwrap());
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
