use crate::app::release::do_payment;
use crate::cli::settings::Settings;
use crate::db::*;
use crate::lightning::LndConnector;
use crate::util::update_order_event;

use chrono::{Duration, Utc};
use mostro_core::order::Status;
use nostr_sdk::{Client, Event};
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::info;

pub async fn start_scheduler(rate_list: Arc<Mutex<Vec<Event>>>, client: &Client) {
    info!("Creating scheduler");

    job_expire_pending_older_orders(client.clone()).await;
    job_update_rate_events(client.clone(), rate_list).await;
    job_cancel_orders(client.clone()).await;
    job_retry_failed_payments().await;

    info!("Scheduler Started");
}

async fn job_retry_failed_payments() {
    let ln_settings = Settings::get_ln();
    let retries_number = ln_settings.payment_retries as i64;
    let interval = ln_settings.payment_retries_interval as u64;

    let pool = crate::db::connect().await.unwrap();

    tokio::spawn(async move {
        loop {
            info!(
                "I run async every {} minutes - checking for failed lighting payment",
                interval
            );

            if let Ok(payment_failed_list) = crate::db::find_failed_payment(&pool).await {
                for payment_failed in payment_failed_list.into_iter() {
                    if payment_failed.payment_attempts < retries_number {
                        let _ = do_payment(payment_failed.clone()).await;
                    }
                }
            }
            tokio::time::sleep(tokio::time::Duration::from_secs(interval)).await;
        }
    });
}

async fn job_update_rate_events(client: Client, rate_list: Arc<Mutex<Vec<Event>>>) {
    // Clone for closure owning with Arc
    let inner_list = rate_list.clone();
    let mostro_settings = Settings::get_mostro();
    let interval = mostro_settings.user_rates_sent_interval_seconds as u64;

    tokio::spawn(async move {
        loop {
            info!(
                "I run async every {} minutes - update rate event of users",
                interval
            );

            for ev in inner_list.lock().await.iter() {
                // Send event to relay
                match client.send_event(ev.clone()).await {
                    Ok(id) => {
                        info!("Updated rate event with id {:?}", id)
                    }
                    Err(e) => {
                        info!("Error on updating rate event {:?}", e.to_string())
                    }
                }
            }

            // Clear list after send events
            inner_list.lock().await.clear();

            let now = Utc::now();
            let next_tick = now
                .checked_add_signed(Duration::seconds(interval as i64))
                .unwrap();
            info!(
                "Next tick for update users rating is {}",
                next_tick.format("%a %b %e %T %Y")
            );

            tokio::time::sleep(tokio::time::Duration::from_secs(interval)).await;
        }
    });
}

async fn job_cancel_orders(client: Client) {
    info!("Create a pool to connect to db");

    let pool = crate::db::connect().await.unwrap();
    let keys = crate::util::get_keys().unwrap();
    let mut ln_client = LndConnector::new().await;
    let mostro_settings = Settings::get_mostro();
    let exp_seconds = mostro_settings.expiration_seconds;

    tokio::spawn(async move {
        loop {
            info!("Check for order to republish for late actions of users");

            if let Ok(older_orders_list) = crate::db::find_order_by_seconds(&pool).await {
                for order in older_orders_list.into_iter() {
                    // Check if order is a sell order and Buyer is not sending the invoice for too much time.
                    // Same if seller is not paying hold invoice
                    if order.status == "WaitingBuyerInvoice" || order.status == "WaitingPayment" {
                        // If hold invoice is paid return funds to seller
                        if order.hash.is_some() {
                            // We return funds to seller
                            let hash = order.hash.as_ref().unwrap();
                            let _ = ln_client.cancel_hold_invoice(hash).await;
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

                        if order.status == "WaitingBuyerInvoice" {
                            if order.kind == "Sell" {
                                // Reset buyer pubkey to none
                                edit_buyer_pubkey_order(&pool, order.id, None)
                                    .await
                                    .unwrap();
                                let _ = edit_master_buyer_pubkey_order(&pool, order.id, None).await;
                            }
                            if order.kind == "Buy" {
                                let _ = edit_seller_pubkey_order(&pool, order.id, None).await;
                                let _ =
                                    edit_master_seller_pubkey_order(&pool, order.id, None).await;
                                new_status = Status::Canceled;
                            };
                            info!("Order Id {}: Reset to status {:?}", &order.id, new_status);
                        };

                        if order.status == "WaitingPayment" {
                            if order.kind == "Sell" {
                                edit_buyer_pubkey_order(&pool, order.id, None)
                                    .await
                                    .unwrap();
                                let _ = edit_master_buyer_pubkey_order(&pool, order.id, None).await;
                                new_status = Status::Canceled;
                            };

                            if order.kind == "Buy" {
                                edit_seller_pubkey_order(&pool, order.id, None)
                                    .await
                                    .unwrap();
                                edit_master_seller_pubkey_order(&pool, order.id, None)
                                    .await
                                    .unwrap();
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
                        let _ = update_order_event(&pool, &client, &keys, new_status, &order).await;
                    }
                }
            }
            let now = Utc::now();
            let next_tick = now
                .checked_add_signed(Duration::seconds(exp_seconds as i64))
                .unwrap();
            info!(
                "Next tick for late action users check is {}",
                next_tick.format("%a %b %e %T %Y")
            );
            tokio::time::sleep(tokio::time::Duration::from_secs(exp_seconds as u64)).await;
        }
    });
}

async fn job_expire_pending_older_orders(client: Client) {
    let pool = crate::db::connect().await.unwrap();
    let keys = crate::util::get_keys().unwrap();

    tokio::spawn(async move {
        loop {
            info!("Check older orders and mark them Expired - check is done every minute");
            if let Ok(older_orders_list) = crate::db::find_order_by_date(&pool).await {
                for order in older_orders_list.iter() {
                    println!("Uid {} - created at {}", order.id, order.created_at);
                    // We update the order id with the new event_id
                    let _res = crate::util::update_order_event(
                        &pool,
                        &client,
                        &keys,
                        Status::Expired,
                        order,
                    )
                    .await;
                }
            }
            let now = Utc::now();
            let next_tick = now.checked_add_signed(Duration::minutes(1)).unwrap();
            info!(
                "Next tick for removal of older orders is {}",
                next_tick.format("%a %b %e %T %Y")
            );
            tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
        }
    });
}
