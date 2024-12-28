use crate::db::Order;
use crate::util::send_message_sync;
use crate::{cli::Commands, db::connect};

use anyhow::Result;
use log::info;
use mostro_core::message::{Action, Message, Payload};
use nostr_sdk::prelude::*;
use std::process;
use uuid::Uuid;

pub async fn execute_send_msg(
    command: Commands,
    order_id: Option<Uuid>,
    identity_keys: Option<&Keys>,
    mostro_key: PublicKey,
    client: &Client,
    text: Option<&str>,
) -> Result<()> {
    // Get desised action based on command from CLI
    let requested_action = match command {
        Commands::FiatSent { order_id: _ } => Action::FiatSent,
        Commands::Release { order_id: _ } => Action::Release,
        Commands::Cancel { order_id: _ } => Action::Cancel,
        Commands::Dispute { order_id: _ } => Action::Dispute,
        Commands::AdmCancel { order_id: _ } => Action::AdminCancel,
        Commands::AdmSettle { order_id: _ } => Action::AdminSettle,
        Commands::AdmAddSolver { npubkey: _ } => Action::AdminAddSolver,
        _ => {
            println!("Not a valid command!");
            process::exit(0);
        }
    };

    println!(
        "Sending {} command for order {:?} to mostro pubId {}",
        requested_action,
        order_id,
        mostro_key.clone()
    );
    let mut payload = None;
    if let Some(t) = text {
        payload = Some(Payload::TextMessage(t.to_string()));
    }

    // Create message
    let message = Message::new_order(order_id, None, None, requested_action, payload);
    info!("Sending message: {:#?}", message);

    let pool = connect().await?;
    let order = Order::get_by_id(&pool, &order_id.unwrap().to_string()).await;
    match order {
        Ok(order) => {
            if let Some(trade_keys_str) = order.trade_keys {
                let trade_keys = Keys::parse(&trade_keys_str)?;
                send_message_sync(
                    client,
                    identity_keys,
                    &trade_keys,
                    mostro_key,
                    message,
                    false,
                    false,
                )
                .await?;
            } else {
                println!("Error: Missing trade keys for order {}", order_id.unwrap());
            }
        }
        Err(e) => {
            println!("Error: {}", e);
        }
    }

    Ok(())
}
