pub mod add_invoice;
pub mod admin_cancel;
pub mod admin_settle;
pub mod cancel;
pub mod dispute;
pub mod fiat_sent;
pub mod order;
pub mod rate_user;
pub mod release;
pub mod take_buy;
pub mod take_sell;

use crate::app::add_invoice::add_invoice_action;
use crate::app::admin_cancel::admin_cancel_action;
use crate::app::admin_settle::admin_settle_action;
use crate::app::cancel::cancel_action;
use crate::app::dispute::dispute_action;
use crate::app::fiat_sent::fiat_sent_action;
use crate::app::order::order_action;
use crate::app::rate_user::update_user_reputation_action;
use crate::app::release::release_action;
use crate::app::take_buy::take_buy_action;
use crate::app::take_sell::take_sell_action;
use crate::lightning::LndConnector;
// use crate::CLEAR_USER_VEC;
use anyhow::Result;
use mostro_core::{Action, Message};
use nostr_sdk::prelude::*;
use sqlx::{Pool, Sqlite};
use std::sync::Arc;
use tokio::sync::Mutex;

pub async fn run(
    my_keys: Keys,
    client: Client,
    ln_client: &mut LndConnector,
    pool: Pool<Sqlite>,
    rate_list: Arc<Mutex<Vec<Event>>>,
) -> Result<()> {
    loop {
        let mut notifications = client.notifications();

        // let mut rate_list: Vec<Event> = vec![];

        // Check if we can send user rates updates
        // if CLEAR_USER_VEC.load(Ordering::Relaxed) {
        //     send_user_rates(&rate_list, &client).await?;
        //     CLEAR_USER_VEC.store(false, Ordering::Relaxed);
        //     rate_list.clear();
        // }

        while let Ok(notification) = notifications.recv().await {
            if let RelayPoolNotification::Event(_, event) = notification {
                if let Kind::EncryptedDirectMessage = event.kind {
                    // We validates if the event is correctly signed
                    event.verify()?;
                    let message = decrypt(
                        &my_keys.secret_key().unwrap(),
                        &event.pubkey,
                        &event.content,
                    );
                    if let Ok(m) = message {
                        let message = Message::from_json(&m);
                        if let Ok(msg) = message {
                            if msg.verify() {
                                match msg.action {
                                    Action::Order => {
                                        order_action(msg, &event, &my_keys, &client, &pool).await?;
                                    }
                                    Action::TakeSell => {
                                        take_sell_action(msg, &event, &my_keys, &client, &pool)
                                            .await?;
                                    }
                                    Action::TakeBuy => {
                                        take_buy_action(msg, &event, &my_keys, &client, &pool)
                                            .await?;
                                    }
                                    Action::FiatSent => {
                                        fiat_sent_action(msg, &event, &my_keys, &client, &pool)
                                            .await?;
                                    }
                                    Action::Release => {
                                        release_action(
                                            msg, &event, &my_keys, &client, &pool, ln_client,
                                        )
                                        .await?;
                                    }
                                    Action::Cancel => {
                                        cancel_action(
                                            msg, &event, &my_keys, &client, &pool, ln_client,
                                        )
                                        .await?;
                                    }
                                    Action::AddInvoice => {
                                        add_invoice_action(msg, &event, &my_keys, &client, &pool)
                                            .await?;
                                    }
                                    Action::PayInvoice => todo!(),
                                    Action::RateUser => {
                                        update_user_reputation_action(
                                            msg,
                                            &event,
                                            &my_keys,
                                            &client,
                                            &pool,
                                            rate_list.clone(),
                                        )
                                        .await?;
                                    }
                                    Action::Dispute => {
                                        dispute_action(msg, &event, &my_keys, &client, &pool)
                                            .await?;
                                    }
                                    Action::AdminCancel => {
                                        admin_cancel_action(
                                            msg, &event, &my_keys, &client, &pool, ln_client,
                                        )
                                        .await?;
                                    }
                                    Action::AdminSettle => {
                                        admin_settle_action(
                                            msg, &event, &my_keys, &client, &pool, ln_client,
                                        )
                                        .await?;
                                    }
                                    _ => todo!(),
                                }
                            }
                        }
                    }
                };
            }
        }
    }
}
