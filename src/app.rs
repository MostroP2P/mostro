pub mod add_invoice;
pub mod admin_add_solver;
pub mod admin_cancel;
pub mod admin_settle;
pub mod admin_take_dispute;
pub mod cancel;
pub mod dispute;
pub mod fiat_sent;
pub mod order;
pub mod rate_user;
pub mod release;
pub mod take_buy;
pub mod take_sell;

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
use crate::lightning::LndConnector;

use anyhow::Result;
use mostro_core::message::{Action, Message};
use nostr_sdk::prelude::*;
use sqlx::{Pool, Sqlite};
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::info;

pub async fn run(
    my_keys: Keys,
    client: &Client,
    ln_client: &mut LndConnector,
    pool: Pool<Sqlite>,
    rate_list: Arc<Mutex<Vec<Event>>>,
) -> Result<()> {
    loop {
        let mut notifications = client.notifications();

        while let Ok(notification) = notifications.recv().await {
            if let RelayPoolNotification::Event { event, .. } = notification {
                if let Kind::EncryptedDirectMessage = event.kind {
                    // We validates if the event is correctly signed
                    event.verify()?;
                    let message = nip04::decrypt(
                        my_keys.secret_key().unwrap(),
                        &event.pubkey,
                        &event.content,
                    );
                    if let Ok(m) = message {
                        let message = Message::from_json(&m);
                        if let Ok(msg) = message {
                            if msg.get_inner_message_kind().verify() {
                                if let Some(action) = msg.inner_action() {
                                    match action {
                                        Action::NewOrder => {
                                            if let Err(e) =
                                                order_action(msg, &event, &my_keys, &pool).await
                                            {
                                                tracing::warn!(
                                                    "Error in {} with context {}",
                                                    action,
                                                    e
                                                );
                                            }
                                        }
                                        Action::TakeSell => {
                                            if let Err(e) =
                                                take_sell_action(msg, &event, &my_keys, &pool).await
                                            {
                                                tracing::warn!(
                                                    "Error in {} with context {}",
                                                    action,
                                                    e
                                                );
                                            }
                                        }
                                        Action::TakeBuy => {
                                            if let Err(e) =
                                                take_buy_action(msg, &event, &my_keys, &pool).await
                                            {
                                                tracing::warn!(
                                                    "Error in {} with context {}",
                                                    action,
                                                    e
                                                );
                                            }
                                        }
                                        Action::FiatSent => {
                                            if let Err(e) =
                                                fiat_sent_action(msg, &event, &my_keys, &pool).await
                                            {
                                                tracing::warn!(
                                                    "Error in {} with context {}",
                                                    action,
                                                    e
                                                );
                                            };
                                        }
                                        Action::Release => {
                                            if let Err(e) = release_action(
                                                msg, &event, &my_keys, &pool, ln_client,
                                            )
                                            .await
                                            {
                                                tracing::warn!(
                                                    "Error in {} with context {}",
                                                    action,
                                                    e
                                                );
                                            }
                                        }
                                        Action::Cancel => {
                                            if let Err(e) = cancel_action(
                                                msg, &event, &my_keys, &pool, ln_client,
                                            )
                                            .await
                                            {
                                                tracing::warn!(
                                                    "Error in {} with context {}",
                                                    action,
                                                    e
                                                );
                                            }
                                        }
                                        Action::AddInvoice => {
                                            if let Err(e) =
                                                add_invoice_action(msg, &event, &my_keys, &pool)
                                                    .await
                                            {
                                                tracing::warn!(
                                                    "Error in {} with context {}",
                                                    action,
                                                    e
                                                );
                                            }
                                        }
                                        Action::PayInvoice => todo!(),
                                        Action::RateUser => {
                                            if let Err(e) = update_user_reputation_action(
                                                msg,
                                                &event,
                                                &my_keys,
                                                &pool,
                                                rate_list.clone(),
                                            )
                                            .await
                                            {
                                                tracing::warn!(
                                                    "Error in {} with context {}",
                                                    action,
                                                    e
                                                );
                                            }
                                        }
                                        Action::Dispute => {
                                            if let Err(e) =
                                                dispute_action(msg, &event, &my_keys, &pool).await
                                            {
                                                tracing::warn!(
                                                    "Error in {} with context {}",
                                                    action,
                                                    e
                                                );
                                            }
                                        }
                                        Action::AdminCancel => {
                                            if let Err(e) = admin_cancel_action(
                                                msg, &event, &my_keys, &pool, ln_client,
                                            )
                                            .await
                                            {
                                                tracing::warn!(
                                                    "Error in {} with context {}",
                                                    action,
                                                    e
                                                );
                                            }
                                        }
                                        Action::AdminSettle => {
                                            if let Err(e) = admin_settle_action(
                                                msg, &event, &my_keys, &pool, ln_client,
                                            )
                                            .await
                                            {
                                                tracing::warn!(
                                                    "Error in {} with context {}",
                                                    action,
                                                    e
                                                );
                                            }
                                        }
                                        Action::AdminAddSolver => {
                                            if let Err(e) = admin_add_solver_action(
                                                msg, &event, &my_keys, &pool,
                                            )
                                            .await
                                            {
                                                tracing::warn!(
                                                    "Error in {} with context {}",
                                                    action,
                                                    e
                                                );
                                            }
                                        }
                                        Action::AdminTakeDispute => {
                                            if let Err(e) =
                                                admin_take_dispute_action(msg, &event, &pool).await
                                            {
                                                tracing::warn!(
                                                    "Error in {} with context {}",
                                                    action,
                                                    e
                                                );
                                            }
                                        }
                                        _ => info!("Received message with action {:?}", action),
                                    }
                                }
                            }
                        }
                    };
                }
            }
        }
    }
}
