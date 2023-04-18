pub mod add_invoice;
pub mod cancel;
pub mod fiat_sent;
pub mod order;
pub mod release;
pub mod take_buy;
pub mod take_sell;
pub mod vote_user;

use crate::app::add_invoice::add_invoice_action;
use crate::app::cancel::cancel_action;
use crate::app::fiat_sent::fiat_sent_action;
use crate::app::order::order_action;
use crate::app::release::release_action;
use crate::app::take_buy::take_buy_action;
use crate::app::take_sell::take_sell_action;
use crate::app::vote_user::update_user_reputation_action;
use crate::lightning::LndConnector;
use anyhow::Result;
use mostro_core::{Action, Message};
use nostr_sdk::prelude::*;
use sqlx::{Pool, Sqlite};

pub async fn run(
    my_keys: Keys,
    client: Client,
    ln_client: &mut LndConnector,
    pool: Pool<Sqlite>,
) -> Result<()> {
    loop {
        let mut notifications = client.notifications();

        while let Ok(notification) = notifications.recv().await {
            if let RelayPoolNotification::Event(_, event) = notification {
                if let Kind::EncryptedDirectMessage = event.kind {
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
                                        order_action(msg, &event, &my_keys, &client, &pool).await?
                                    }
                                    Action::TakeSell => {
                                        take_sell_action(msg, &event, &my_keys, &client, &pool)
                                            .await?
                                    }
                                    Action::TakeBuy => {
                                        take_buy_action(msg, &event, &my_keys, &client, &pool)
                                            .await?
                                    }
                                    Action::FiatSent => {
                                        fiat_sent_action(msg, &event, &my_keys, &client, &pool)
                                            .await?
                                    }
                                    Action::Release => {
                                        release_action(
                                            msg, &event, &my_keys, &client, &pool, ln_client,
                                        )
                                        .await?
                                    }
                                    Action::Cancel => {
                                        cancel_action(
                                            msg, &event, &my_keys, &client, &pool, ln_client,
                                        )
                                        .await?
                                    }
                                    Action::AddInvoice => {
                                        add_invoice_action(msg, &event, &my_keys, &client, &pool)
                                            .await?
                                    }
                                    Action::PayInvoice => todo!(),
                                    Action::VoteUser => {
                                        update_user_reputation_action(msg, &event, &my_keys, &client,&pool).await?;
                                    },
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
