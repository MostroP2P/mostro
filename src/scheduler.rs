use crate::app::release::do_payment;
use crate::bitcoin_price::BitcoinPriceManager;
use crate::config;
use crate::db::*;
use crate::lightning::LndConnector;
use crate::util;
use crate::util::get_nostr_client;
use crate::LN_STATUS;
use crate::{Keys, PublicKey};

use chrono::{TimeDelta, Utc};
use config::*;
use mostro_core::prelude::*;
use nostr_sdk::EventBuilder;
use nostr_sdk::{Kind as NostrKind, Tag};
use sqlx_crud::Crud;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{error, info};
use util::{get_keys, get_nostr_relays, send_dm, update_order_event};

pub async fn start_scheduler() {
    info!("Creating scheduler");

    job_expire_pending_older_orders().await;
    job_update_rate_events().await;
    job_cancel_orders().await;
    job_retry_failed_payments().await;
    job_info_event_send().await;
    job_relay_list().await;
    job_update_bitcoin_prices().await;
    job_flush_messages_queue().await;

    info!("Scheduler Started");
}

async fn job_flush_messages_queue() {
    // Clone for closure owning with Arc
    let order_msg_list = MESSAGE_QUEUES.queue_order_msg.clone();
    // Clone for closure owning with Arc
    let cantdo_msg_list = MESSAGE_QUEUES.queue_order_cantdo.clone();
    let sender_keys = match get_keys() {
        Ok(keys) => keys,
        Err(e) => return error!("{e}"),
    };

    // Helper function to send messages
    async fn send_messages(
        msg_list: Arc<RwLock<Vec<(Message, PublicKey)>>>,
        sender_keys: Keys,
        retries: &mut usize,
    ) {
        if !msg_list.read().await.is_empty() {
            let (message, destination_key) = msg_list.read().await[0].clone();
            match message.as_json() {
                Ok(msg) => {
                    if let Err(e) = send_dm(destination_key, &sender_keys, &msg, None).await {
                        error!("Failed to send message: {}", e);
                        *retries += 1;
                    } else {
                        *retries = 0;
                        msg_list.write().await.remove(0);
                    }
                }
                Err(e) => error!("Failed to parse message: {}", e),
            }
            if *retries > 3 {
                *retries = 0; // Reset retries after removing message
                msg_list.write().await.remove(0);
            }
        }
    }

    // Spawn a new task to flush the messages queue
    tokio::spawn(async move {
        let mut retries_messages = 0;
        let mut retries_cantdo_messages = 0;

        loop {
            send_messages(
                order_msg_list.clone(),
                sender_keys.clone(),
                &mut retries_messages,
            )
            .await;
            send_messages(
                cantdo_msg_list.clone(),
                sender_keys.clone(),
                &mut retries_cantdo_messages,
            )
            .await;

            tokio::time::sleep(tokio::time::Duration::from_millis(250)).await;
        }
    });
}

async fn job_relay_list() {
    let mostro_keys = match get_keys() {
        Ok(keys) => keys,
        Err(e) => return error!("{e}"),
    };
    let client = match get_nostr_client() {
        Ok(client) => client,
        Err(e) => return error!("{e}"),
    };

    tokio::spawn(async move {
        loop {
            info!("Sending Mostro relay list");

            let interval = Settings::get_mostro().publish_relays_interval as u64;
            if let Some(relays) = get_nostr_relays().await {
                let mut relay_tags: Vec<Tag> = vec![];

                for (_, r) in relays.iter() {
                    if r.is_connected() {
                        relay_tags.push(Tag::relay_metadata(r.url().clone(), None))
                    }
                }
                if let Ok(relay_ev) = EventBuilder::new(NostrKind::RelayList, "")
                    .tags(relay_tags)
                    .sign_with_keys(&mostro_keys)
                {
                    let _ = client.send_event(&relay_ev).await;
                }
            }
            tokio::time::sleep(tokio::time::Duration::from_secs(interval)).await;
        }
    });
}

async fn job_info_event_send() {
    let mostro_keys = match get_keys() {
        Ok(keys) => keys,
        Err(e) => return error!("{e}"),
    };
    let client = match get_nostr_client() {
        Ok(client) => client,
        Err(e) => return error!("{e}"),
    };
    let interval = Settings::get_mostro().publish_mostro_info_interval as u64;
    let ln_status = LN_STATUS.get().unwrap();
    tokio::spawn(async move {
        loop {
            info!("Sending info about mostro");

            let tags = crate::nip33::info_to_tags(ln_status);
            let id = mostro_keys.public_key().to_string();

            let info_ev = match crate::nip33::new_event(&mostro_keys, "", id, tags) {
                Ok(info) => info,
                Err(e) => return error!("{e}"),
            };

            let _ = client.send_event(&info_ev).await;

            tokio::time::sleep(tokio::time::Duration::from_secs(interval)).await;
        }
    });
}

async fn job_retry_failed_payments() {
    let ln_settings = Settings::get_ln();
    let retries_number = ln_settings.payment_attempts as i64;
    let interval = ln_settings.payment_retries_interval as u64;

    // Arc clone db pool to safe use across threads
    let pool = get_db_pool();

    tokio::spawn(async move {
        loop {
            info!(
                "I run async every {} minutes - checking for failed lighting payment",
                interval
            );

            if let Ok(payment_failed_list) = crate::db::find_failed_payment(&pool).await {
                for payment_failed in payment_failed_list.into_iter() {
                    if payment_failed.payment_attempts < retries_number {
                        if let Err(e) = do_payment(payment_failed.clone(), None).await {
                            error!("{e}");
                        }
                    }
                }
            }
            tokio::time::sleep(tokio::time::Duration::from_secs(interval)).await;
        }
    });
}

async fn job_update_rate_events() {
    // Clone for closure owning with Arc
    let queue_order_rate = MESSAGE_QUEUES.queue_order_rate.clone();
    let mostro_settings = Settings::get_mostro();
    let interval = mostro_settings.user_rates_sent_interval_seconds as u64;
    let client = match get_nostr_client() {
        Ok(client) => client,
        Err(e) => return error!("{e}"),
    };

    tokio::spawn(async move {
        loop {
            info!(
                "I run async every {} minutes - update rate event of users",
                interval / 60
            );

            for ev in queue_order_rate.read().await.iter() {
                // Send event to relay
                let _ = client.send_event(&ev.clone()).await;
            }

            // Clear list after send events
            queue_order_rate.write().await.clear();

            let now = Utc::now();
            if let Some(next_tick) = now.checked_add_signed(
                TimeDelta::try_seconds(interval as i64).expect("Wrong seconds value"),
            ) {
                info!(
                    "Next tick for update users rating is {}",
                    next_tick.format("%a %b %e %T %Y")
                );
            }

            tokio::time::sleep(tokio::time::Duration::from_secs(interval)).await;
        }
    });
}

async fn job_cancel_orders() {
    info!("Create a pool to connect to db");

    // Arc clone db pool to safe use across threads
    let pool = get_db_pool();

    let keys = match get_keys() {
        Ok(keys) => keys,
        Err(e) => {
            return error!("{e}");
        }
    };

    let mut ln_client = if let Ok(client) = LndConnector::new().await {
        client
    } else {
        return error!("Failed to create LND client");
    };
    let mostro_settings = Settings::get_mostro();
    let exp_seconds = mostro_settings.expiration_seconds;

    tokio::spawn(async move {
        loop {
            info!("Check for order to republish for late actions of users");

            if let Ok(older_orders_list) = crate::db::find_order_by_seconds(&pool).await {
                for order in older_orders_list.into_iter() {
                    // Check if order is a sell order and Buyer is not sending the invoice for too much time.
                    // Same if seller is not paying hold invoice
                    if order.status == Status::WaitingBuyerInvoice.to_string()
                        || order.status == Status::WaitingPayment.to_string()
                    {
                        // If hold invoice is paid return funds to seller
                        // We return funds to seller
                        if let Some(hash) = order.hash.as_ref() {
                            if let Err(e) = ln_client.cancel_hold_invoice(hash).await {
                                error!("{e}");
                            }
                            info!("Order Id {}: Funds returned to seller - buyer did not sent regular invoice in time", &order.id);
                        };
                        let mut order = order.clone();
                        // We re-publish the event with Pending status
                        // and update on local database
                        if order.price_from_api {
                            order.amount = 0;
                            order.fee = 0;
                        }

                        // Initialize reset status to pending, change in case of specifici needs of order
                        let mut new_status = Status::Pending;

                        if order.status == Status::WaitingBuyerInvoice.to_string() {
                            if order.kind == Kind::Sell.to_string() {
                                // Reset buyer pubkey to none
                                if let Err(e) = edit_buyer_pubkey_order(&pool, order.id, None).await
                                {
                                    error!("{e}");
                                }
                                if let Err(e) =
                                    edit_master_buyer_pubkey_order(&pool, order.id, None).await
                                {
                                    error!("{e}");
                                }
                            }
                            if order.kind == Kind::Buy.to_string() {
                                if let Err(e) =
                                    edit_seller_pubkey_order(&pool, order.id, None).await
                                {
                                    error!("{e}");
                                }
                                if let Err(e) =
                                    edit_master_seller_pubkey_order(&pool, order.id, None).await
                                {
                                    error!("{e}");
                                }
                                new_status = Status::Canceled;
                            };
                            info!("Order Id {}: Reset to status {:?}", &order.id, new_status);
                        };

                        if order.status == Status::WaitingPayment.to_string() {
                            if order.kind == Kind::Sell.to_string() {
                                if let Err(e) = edit_buyer_pubkey_order(&pool, order.id, None).await
                                {
                                    error!("{e}");
                                }
                                if let Err(e) =
                                    edit_master_buyer_pubkey_order(&pool, order.id, None).await
                                {
                                    error!("{e}");
                                }
                                new_status = Status::Canceled;
                            };

                            if order.kind == Kind::Buy.to_string() {
                                if let Err(e) =
                                    edit_seller_pubkey_order(&pool, order.id, None).await
                                {
                                    error!("{e}");
                                }
                                if let Err(e) =
                                    edit_master_seller_pubkey_order(&pool, order.id, None).await
                                {
                                    error!("{e}");
                                }
                            };
                            info!("Order Id {}: Reset to status {:?}", &order.id, new_status);
                        }
                        if new_status == Status::Pending {
                            let _ = update_order_to_initial_state(
                                &pool,
                                order.id,
                                order.amount,
                                order.fee,
                            )
                            .await;
                            info!(
                                "Republishing order Id {}, not received regular invoice in time",
                                order.id
                            );
                        } else {
                            info!(
                                "Canceled order Id {}, not received regular invoice in time",
                                order.id
                            );
                        }
                        if let Ok(order_updated) =
                            update_order_event(&keys, new_status, &order).await
                        {
                            let _ = order_updated.update(&pool).await;
                        }
                    }
                }
            }
            let now = Utc::now();
            if let Some(next_tick) = now.checked_add_signed(
                TimeDelta::try_seconds(exp_seconds as i64).expect("Wrong seconds value"),
            ) {
                info!(
                    "Next tick for late action users check is {}",
                    next_tick.format("%a %b %e %T %Y")
                );
            }
            tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
        }
    });
}

async fn job_expire_pending_older_orders() {
    let pool = match connect().await {
        Ok(p) => p,
        Err(e) => return error!("{e}"),
    };
    let keys = match get_keys() {
        Ok(keys) => keys,
        Err(e) => return error!("{e}"),
    };

    tokio::spawn(async move {
        loop {
            info!("Check older orders and mark them Expired - check is done every minute");
            if let Ok(older_orders_list) = crate::db::find_order_by_date(&pool).await {
                for order in older_orders_list.iter() {
                    tracing::info!(
                        "Order id {} - created at {} is expired",
                        order.id,
                        order.created_at
                    );
                    // We update the order id with the new event_id
                    if let Ok(order_updated) =
                        crate::util::update_order_event(&keys, Status::Expired, order).await
                    {
                        let _ = order_updated.update(&pool).await;
                    }
                }
            }
            let now = Utc::now();
            if let Some(next_tick) =
                now.checked_add_signed(TimeDelta::try_minutes(1).expect("Wrong minutes value"))
            {
                info!(
                    "Next tick for removal of older orders is {}",
                    next_tick.format("%a %b %e %T %Y")
                );
            }
            tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
        }
    });
}

async fn job_update_bitcoin_prices() {
    tokio::spawn(async {
        loop {
            info!("Updating Bitcoin prices");
            if let Err(e) = BitcoinPriceManager::update_prices().await {
                error!("Failed to update Bitcoin prices: {}", e);
            }
            tokio::time::sleep(tokio::time::Duration::from_secs(300)).await;
        }
    });
}
