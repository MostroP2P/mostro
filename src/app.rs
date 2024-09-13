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
use crate::nip59::unwrap_gift_wrap;
use crate::Settings;

use anyhow::Result;
use mostro_core::message::{Action, Message};
use nostr_sdk::prelude::*;
use sqlx::{Pool, Sqlite};
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::error;
use tracing::info;

fn warning_msg(action: &Action, e: anyhow::Error) {
    tracing::warn!("Error in {} with context {}", action, e);
}

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
                // Verify pow
                if !event.check_pow(pow) {
                    // Discard
                    info!("Not POW verified event!");
                    continue;
                }
                if let Kind::GiftWrap = event.kind {
                    // We validates if the event is correctly signed
                    if event.verify().is_err() {
                        tracing::warn!("Error in event verification")
                    };

                    let event = unwrap_gift_wrap(&my_keys, &event)?;

                    let message = Message::from_json(&event.rumor.content);
                    match message {
                        Ok(msg) => {
                            if msg.get_inner_message_kind().verify() {
                                if let Some(action) = msg.inner_action() {
                                    match action {
                                        Action::NewOrder => {
                                            if let Err(e) =
                                                order_action(msg, &event, &my_keys, &pool).await
                                            {
                                                warning_msg(&action, e)
                                            }
                                        }
                                        Action::TakeSell => {
                                            if let Err(e) =
                                                take_sell_action(msg, &event, &my_keys, &pool).await
                                            {
                                                warning_msg(&action, e)
                                            }
                                        }
                                        Action::TakeBuy => {
                                            if let Err(e) =
                                                take_buy_action(msg, &event, &my_keys, &pool).await
                                            {
                                                warning_msg(&action, e)
                                            }
                                        }
                                        Action::FiatSent => {
                                            if let Err(e) =
                                                fiat_sent_action(msg, &event, &my_keys, &pool).await
                                            {
                                                warning_msg(&action, e)
                                            }
                                        }
                                        Action::Release => {
                                            if let Err(e) = release_action(
                                                msg, &event, &my_keys, &pool, ln_client,
                                            )
                                            .await
                                            {
                                                warning_msg(&action, e)
                                            }
                                        }
                                        Action::Cancel => {
                                            if let Err(e) = cancel_action(
                                                msg, &event, &my_keys, &pool, ln_client,
                                            )
                                            .await
                                            {
                                                warning_msg(&action, e)
                                            }
                                        }
                                        Action::AddInvoice => {
                                            if let Err(e) =
                                                add_invoice_action(msg, &event, &my_keys, &pool)
                                                    .await
                                            {
                                                warning_msg(&action, e)
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
                                                warning_msg(&action, e)
                                            }
                                        }
                                        Action::Dispute => {
                                            if let Err(e) =
                                                dispute_action(msg, &event, &my_keys, &pool).await
                                            {
                                                warning_msg(&action, e)
                                            }
                                        }
                                        Action::AdminCancel => {
                                            if let Err(e) = admin_cancel_action(
                                                msg, &event, &my_keys, &pool, ln_client,
                                            )
                                            .await
                                            {
                                                warning_msg(&action, e)
                                            }
                                        }
                                        Action::AdminSettle => {
                                            if let Err(e) = admin_settle_action(
                                                msg, &event, &my_keys, &pool, ln_client,
                                            )
                                            .await
                                            {
                                                warning_msg(&action, e)
                                            }
                                        }
                                        Action::AdminAddSolver => {
                                            if let Err(e) = admin_add_solver_action(
                                                msg, &event, &my_keys, &pool,
                                            )
                                            .await
                                            {
                                                warning_msg(&action, e)
                                            }
                                        }
                                        Action::AdminTakeDispute => {
                                            if let Err(e) =
                                                admin_take_dispute_action(msg, &event, &pool).await
                                            {
                                                warning_msg(&action, e)
                                            }
                                        }
                                        _ => info!("Received message with action {:?}", action),
                                    }
                                }
                            }
                        }
                        Err(e) => error!("Failed to parse message from JSON: {:?}", e),
                    }
                }
            }
        }
    }
}
