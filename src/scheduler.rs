use crate::app::release::do_payment;
use crate::cli::settings::Settings;
use crate::db::*;
use crate::lightning::LndConnector;
use crate::util;
use crate::NOSTR_CLIENT;

use chrono::{TimeDelta, Utc};
use mostro_core::order::{Kind, Status};
use nostr_sdk::Event;
use sqlx_crud::Crud;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{error, info};
use util::{get_keys, update_order_event};

pub async fn start_scheduler(rate_list: Arc<Mutex<Vec<Event>>>) {
    info!("Creating scheduler");

    job_expire_pending_older_orders().await;
    job_update_rate_events(rate_list).await;
    job_cancel_orders().await;
    job_retry_failed_payments().await;

    info!("Scheduler Started");
}

async fn job_retry_failed_payments() {
    let ln_settings = Settings::get_ln();
    let retries_number = ln_settings.payment_attempts as i64;
    let interval = ln_settings.payment_retries_interval as u64;

    let pool = match connect().await {
        Ok(p) => p,
        Err(e) => return error!("{e}"),
    };

    tokio::spawn(async move {
        loop {
            info!(
                "I run async every {} minutes - checking for failed lighting payment",
                interval
            );

            if let Ok(payment_failed_list) = crate::db::find_failed_payment(&pool).await {
                for payment_failed in payment_failed_list.into_iter() {
                    if payment_failed.payment_attempts < retries_number {
                        if let Err(e) = do_payment(payment_failed.clone()).await {
                            error!("{e}");
                        }
                    }
                }
            }
            tokio::time::sleep(tokio::time::Duration::from_secs(interval)).await;
        }
    });
}

async fn job_update_rate_events(rate_list: Arc<Mutex<Vec<Event>>>) {
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
                if let Some(client) = NOSTR_CLIENT.get() {
                    match &client.send_event(ev.clone()).await {
                        Ok(id) => {
                            info!("Updated rate event with id {:?}", id)
                        }
                        Err(e) => {
                            info!("Error on updating rate event {:?}", e.to_string())
                        }
                    }
                }
            }

            // Clear list after send events
            inner_list.lock().await.clear();

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

    let pool = match connect().await {
        Ok(p) => p,
        Err(e) => return error!("{e}"),
    };
    let keys = match get_keys() {
        Ok(keys) => keys,
        Err(e) => return error!("{e}"),
    };

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
            tokio::time::sleep(tokio::time::Duration::from_secs(exp_seconds as u64)).await;
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
                    println!("Uid {} - created at {}", order.id, order.created_at);
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
