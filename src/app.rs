//! Main application module for the P2P trading system.
//! Handles message routing, action processing, and event loop management.

// Submodules for different trading actions
pub mod add_invoice; // Handles invoice creation
pub mod admin_add_solver; // Admin functionality to add dispute solvers
pub mod admin_cancel; // Admin order cancellation
pub mod admin_settle; // Admin dispute settlement
pub mod admin_take_dispute; // Admin dispute handling
pub mod cancel; // User order cancellation
pub mod dispute; // User dispute handling
pub mod fiat_sent; // Fiat payment confirmation
pub mod order; // Order creation and management
pub mod rate_user; // User reputation system
pub mod release; // Release of held funds
pub mod take_buy; // Taking buy orders
pub mod take_sell; // Taking sell orders

// Import action handlers from submodules
use crate::app::add_invoice::add_invoice_action;
use crate::app::admin_add_solver::admin_add_solver_action;
use crate::app::admin_cancel::admin_cancel_action;
use crate::app::admin_settle::admin_settle_action;
use crate::app::admin_take_dispute::admin_take_dispute_action;
use crate::app::cancel::cancel_action;
use crate::app::dispute::dispute_action;
use crate::app::fiat_sent::fiat_sent_action;
use crate::app::order::order_action;
use crate::app::rate_user::update_user_reputation_action;
use crate::app::release::release_action;
use crate::app::take_buy::take_buy_action;
use crate::app::take_sell::take_sell_action;
use crate::db::update_user_trade_index;
// Core functionality imports
use crate::db::add_new_user;
use crate::db::is_user_present;
use crate::lightning::LndConnector;
use crate::util::send_cant_do_msg;
use crate::Settings;

// External dependencies
use anyhow::Result;
use mostro_core::message::{Action, CantDoReason, Message};
use mostro_core::user::User;
use nostr_sdk::prelude::*;
use sqlx::{Pool, Sqlite};
use std::sync::Arc;
use tokio::sync::Mutex;
/// Helper function to log warning messages for action errors
fn warning_msg(action: &Action, e: anyhow::Error) {
    tracing::warn!("Error in {} with context {}", action, e);
}

/// Function to check if a user is present in the database and update or create their trade index.
///
/// This function performs the following tasks:
/// 1. It checks if the action associated with the incoming message is related to trading (NewOrder, TakeBuy, or TakeSell).
/// 2. If the user is found in the database, it verifies the trade index and the signature of the message.
///    - If valid, it updates the user's trade index.
///    - If invalid, it logs a warning and sends a message indicating the issue.
/// 3. If the user is not found, it creates a new user entry with the provided trade index if applicable.
///
/// # Arguments
/// * `pool` - The database connection pool used to query and update user data.
/// * `event` - The unwrapped gift event containing the sender's information.
/// * `msg` - The message containing action details and trade index information.
async fn check_trade_index(pool: &Pool<Sqlite>, event: &UnwrappedGift, msg: &Message) {
    let message_kind = msg.get_inner_message_kind();

    // Only process actions related to trading
    if !matches!(
        message_kind.action,
        Action::NewOrder | Action::TakeBuy | Action::TakeSell
    ) {
        return;
    }

    // If user is present, we check the trade index and signature
    match is_user_present(pool, event.sender.to_string()).await {
        Ok(user) => {
            if let (true, index) = message_kind.has_trade_index() {
                let content: (Message, Signature) = match serde_json::from_str::<(
                    Message,
                    nostr_sdk::secp256k1::schnorr::Signature,
                )>(&event.rumor.content)
                {
                    Ok(data) => data,
                    Err(e) => {
                        tracing::error!("Error deserializing content: {}", e);
                        return;
                    }
                };

                let (_, sig) = content;

                if index <= user.last_trade_index {
                    tracing::info!("Invalid trade index");
                    send_cant_do_msg(
                        None,
                        message_kind.id,
                        Some(CantDoReason::InvalidTradeIndex),
                        &event.rumor.pubkey,
                    )
                    .await;
                    return;
                }

                if !message_kind.verify_signature(event.rumor.pubkey, sig) {
                    tracing::info!("Invalid signature");
                    send_cant_do_msg(
                        None,
                        message_kind.id,
                        Some(CantDoReason::InvalidSignature),
                        &event.rumor.pubkey,
                    )
                    .await;
                    return;
                }

                if let Err(e) = update_user_trade_index(pool, event.sender.to_string(), index).await
                {
                    tracing::error!("Error updating user trade index: {}", e);
                }
            }
        }
        Err(_) => {
            if let (true, last_trade_index) = message_kind.has_trade_index() {
                let new_user: User = User {
                    pubkey: event.sender.to_string(),
                    last_trade_index,
                    ..Default::default()
                };
                if let Err(e) = add_new_user(pool, new_user).await {
                    tracing::error!("Error creating new user: {}", e);
                    send_cant_do_msg(
                        None,
                        msg.get_inner_message_kind().id,
                        Some(CantDoReason::CantCreateUser),
                        &event.rumor.pubkey,
                    )
                    .await;
                }
            }
        }
    }
}

/// Handles the processing of a single message action by routing it to the appropriate handler
/// based on the action type. This is the core message routing logic of the application.
///
/// # Arguments
/// * `action` - The type of action to be performed
/// * `msg` - The message containing action details
/// * `event` - The unwrapped gift wrap event
/// * `my_keys` - Node keypair for signing/verification
/// * `pool` - Database connection pool
/// * `ln_client` - Lightning network connector
/// * `rate_list` - Shared list of rating events
async fn handle_message_action(
    action: &Action,
    msg: Message,
    event: &UnwrappedGift,
    my_keys: &Keys,
    pool: &Pool<Sqlite>,
    ln_client: &mut LndConnector,
    rate_list: Arc<Mutex<Vec<Event>>>,
) -> Result<()> {
    match action {
        // Order-related actions
        Action::NewOrder => order_action(msg, event, my_keys, pool).await,
        Action::TakeSell => take_sell_action(msg, event, my_keys, pool).await,
        Action::TakeBuy => take_buy_action(msg, event, my_keys, pool).await,

        // Payment-related actions
        Action::FiatSent => fiat_sent_action(msg, event, my_keys, pool).await,
        Action::Release => release_action(msg, event, my_keys, pool, ln_client).await,
        Action::AddInvoice => add_invoice_action(msg, event, my_keys, pool).await,
        Action::PayInvoice => todo!(),

        // Dispute and rating actions
        Action::Dispute => dispute_action(msg, event, my_keys, pool).await,
        Action::RateUser => {
            update_user_reputation_action(msg, event, my_keys, pool, rate_list).await
        }
        Action::Cancel => cancel_action(msg, event, my_keys, pool, ln_client).await,

        // Admin actions
        Action::AdminCancel => admin_cancel_action(msg, event, my_keys, pool, ln_client).await,
        Action::AdminSettle => admin_settle_action(msg, event, my_keys, pool, ln_client).await,
        Action::AdminAddSolver => admin_add_solver_action(msg, event, my_keys, pool).await,
        Action::AdminTakeDispute => admin_take_dispute_action(msg, event, pool).await,

        _ => {
            tracing::info!("Received message with action {:?}", action);
            Ok(())
        }
    }
}

/// Main event loop that processes incoming Nostr events.
/// Handles message verification, POW checking, and routes valid messages to appropriate handlers.
///
/// # Arguments
/// * `my_keys` - The node's keypair
/// * `client` - Nostr client instance
/// * `ln_client` - Lightning network connector
/// * `pool` - SQLite connection pool
/// * `rate_list` - Shared list of rating events
pub async fn run(
    my_keys: Keys,
    client: &Client,
    ln_client: &mut LndConnector,
    pool: Pool<Sqlite>,
    rate_list: Arc<Mutex<Vec<Event>>>,
) -> Result<()> {
    loop {
        let mut notifications = client.notifications();

        // Get pow from config
        let pow = Settings::get_mostro().pow;
        while let Ok(notification) = notifications.recv().await {
            if let RelayPoolNotification::Event { event, .. } = notification {
                // Verify proof of work
                if !event.check_pow(pow) {
                    // Discard events that don't meet POW requirements
                    tracing::info!("Not POW verified event!");
                    continue;
                }
                if let Kind::GiftWrap = event.kind {
                    // Validate event signature
                    if event.verify().is_err() {
                        tracing::warn!("Error in event verification")
                    };

                    let event = match nip59::extract_rumor(&my_keys, &event).await {
                        Ok(u) => u,
                        Err(_) => {
                            println!("Error unwrapping gift");
                            continue;
                        }
                    };
                    // Discard events older than 10 seconds to prevent replay attacks
                    let since_time = chrono::Utc::now()
                        .checked_sub_signed(chrono::Duration::seconds(10))
                        .unwrap()
                        .timestamp() as u64;
                    if event.rumor.created_at.as_u64() < since_time {
                        continue;
                    }

                    let (message, sig): (Message, Option<Signature>) =
                        match serde_json::from_str(&event.rumor.content) {
                            Ok(data) => data,
                            Err(e) => {
                                tracing::error!("Error deserializing content: {}", e);
                                continue;
                            }
                        };
                    let inner_message = message.get_inner_message_kind();

                    let sender_matches_rumor = event.sender == event.rumor.pubkey;

                    if let Some(sig) = sig {
                        // Verify signature only if sender and rumor pubkey are different
                        if !sender_matches_rumor
                            && !inner_message.verify_signature(event.rumor.pubkey, sig)
                        {
                            tracing::warn!("Error in event verification");
                            continue;
                        }
                    } else if !sender_matches_rumor {
                        // If there is no signature and the sender does not match the rumor pubkey, there is also an error
                        tracing::warn!("Error in event verification");
                        continue;
                    }

                    // Check if message is message with trade index
                    check_trade_index(&pool, &event, &message).await;

                    if inner_message.verify() {
                        if let Some(action) = message.inner_action() {
                            if let Err(e) = handle_message_action(
                                &action,
                                message,
                                &event,
                                &my_keys,
                                &pool,
                                ln_client,
                                rate_list.clone(),
                            )
                            .await
                            {
                                warning_msg(&action, e)
                            }
                        }
                    }
                }
            }
        }
    }
}
