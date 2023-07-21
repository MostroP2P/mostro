use crate::db::*;
use crate::lightning::LndConnector;
use crate::settings::Settings;
use crate::util::update_order_event;
use crate::RATE_EVENT_LIST;

use anyhow::Result;
use mostro_core::Status;
use std::error::Error;
use tokio_cron_scheduler::{Job, JobScheduler};
use tracing::{info, warn, Level};
use tracing_subscriber::FmtSubscriber;

pub async fn start_scheduler() -> Result<JobScheduler, Box<dyn Error>> {
    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::INFO)
        .finish();
    tracing::subscriber::set_global_default(subscriber).expect("Setting default subscriber failed");
    info!("Creating scheduler");
    let sched = JobScheduler::new().await?;
    cron_scheduler(&sched).await?;

    Ok(sched)
}

pub async fn cron_scheduler(sched: &JobScheduler) -> Result<(), anyhow::Error> {
    // This job find older Pending orders and mark them Expired
    let job_expire_pending_older_orders = Job::new_async("0 * * * * *", move |uuid, mut l| {
        Box::pin(async move {
            info!("Create a pool to connect to db");
            let pool = crate::db::connect().await.unwrap();
            // Connect to relays
            let client = crate::util::connect_nostr().await.unwrap();
            let keys = crate::util::get_keys().unwrap();

            info!("Check older orders and mark them Expired - check is done every minute");

            let older_orders_list = crate::db::find_order_by_date(&pool).await;

            for order in older_orders_list.unwrap().iter() {
                println!("Uid {} - created at {}", order.id, order.created_at);
                // We update the order id with the new event_id
                let _res = crate::util::update_order_event(
                    &pool,
                    &client,
                    &keys,
                    mostro_core::Status::Expired,
                    order,
                    None,
                )
                .await;
            }
            let next_tick = l.next_tick_for_job(uuid).await;
            match next_tick {
                Ok(Some(ts)) => info!("Next time for 1 minute is {:?}", ts),
                _ => warn!("Could not get next tick for job"),
            }
        })
    })
    .unwrap();

    // This job is used to cancel or republish pending orders that are not updated for more than EXP_SECONDS seconds
    let job_cancel_orders = Job::new_async("0 * * * * *", move |uuid, mut l| {
        Box::pin(async move {
            info!("Create a pool to connect to db");
            let pool = crate::db::connect().await.unwrap();
            // Connect to relays
            let client = crate::util::connect_nostr().await.unwrap();
            let keys = crate::util::get_keys().unwrap();
            let mut ln_client = LndConnector::new().await;
            let mostro_settings = Settings::get_mostro().unwrap();
            let exp_seconds = mostro_settings.expiration_seconds;

            info!("Check for order to republish for late actions of users");

            let older_orders_list = crate::db::find_order_by_seconds(&pool).await;

            for order in older_orders_list.unwrap().into_iter() {
                // Check if order is a sell order and Buyer is not sending the invoice for too much time.
                // Same if seller is not paying hold invoice
                if order.status == "WaitingBuyerInvoice" || order.status == "WaitingPayment" {
                    // If hold invoice is payed return funds to seller
                    if order.hash.is_some() {
                        // We return funds to seller
                        let hash = order.hash.as_ref().unwrap();
                        ln_client.cancel_hold_invoice(hash).await.unwrap();
                        info!("Order Id {}: Funds returned to seller - buyer did not sent regular invoice in time", &order.id);
                    };
                    // We re-publish the event with Pending status
                    // and update on local database
                    let mut updated_order_amount = order.amount;
                    let mut updated_order_fee = order.fee;
                    if order.price_from_api {
                        updated_order_amount = 0;
                        updated_order_fee = 0;
                    }

                    // Initialize reset status to pending, change in case of specifici needs of order
                    let mut new_status = Status::Pending;

                    if order.status == "WaitingBuyerInvoice" {
                        if order.kind == "Sell"{
                            // Reset buyer pubkey to none
                            edit_buyer_pubkey_order(&pool,
                                order.id,
                                None)
                                .await.unwrap();
                            edit_master_buyer_pubkey_order(&pool, order.id, None).await.unwrap();
                        }
                        if order.kind == "Buy"{
                            edit_seller_pubkey_order(&pool, order.id, None).await.unwrap();
                            edit_master_seller_pubkey_order(&pool, order.id, None).await.unwrap();
                            new_status = Status::Canceled;
                        };
                        info!("Order Id {}: Reset to status {:?}", &order.id, new_status);
                    };

                    if order.status == "WaitingPayment" {
                        if order.kind == "Sell"{
                            edit_buyer_pubkey_order(&pool,
                            order.id,
                            None)
                            .await.unwrap();
                            edit_master_buyer_pubkey_order(&pool, order.id, None).await.unwrap();
                            new_status = Status::Canceled;
                        };

                        if order.kind == "Buy"{
                            edit_seller_pubkey_order(&pool, order.id, None).await.unwrap();
                            edit_master_seller_pubkey_order(&pool, order.id, None).await.unwrap();
                        };
                        info!("Order Id {}: Reset to status {:?}", &order.id, new_status);
                    }
                    if new_status == Status::Pending {
                        update_order_to_initial_state(&pool,order.id,updated_order_amount,updated_order_fee).await.unwrap();
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
                    update_order_event(
                        &pool,
                        &client,
                        &keys,
                        new_status,
                        &order,
                        Some(updated_order_amount)
                    ).await.unwrap();
                }

            }
            let next_tick = l.next_tick_for_job(uuid).await;
            match next_tick {
                Ok(Some(ts)) => info!("Checking orders stuck for more than {} minutes - next check is at {:?}",exp_seconds.to_string(), ts ),
                _ => warn!("Could not get next tick for job"),
            }
        })
    })
    .unwrap();

    let job_update_rate_events = Job::new_async("0 0 * * * *", move |uuid, mut l| {
        Box::pin(async move {
            // Connect to relays
            let client = crate::util::connect_nostr().await.unwrap();

            info!("I run async every hour - update rate event of users",);

            for ev in RATE_EVENT_LIST.lock().await.iter() {
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
            // Clear list
            RATE_EVENT_LIST.lock().await.clear();

            let next_tick = l.next_tick_for_job(uuid).await;
            match next_tick {
                Ok(Some(ts)) => info!("Next time for 1 hour is {:?}", ts),
                _ => warn!("Could not get next tick for job"),
            }
        })
    })
    .unwrap();

    // Add the task to the scheduler
    sched.add(job_expire_pending_older_orders).await?;
    sched.add(job_cancel_orders).await?;
    sched.add(job_update_rate_events).await?;

    Ok(())
}
