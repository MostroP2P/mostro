use crate::db::{edit_buyer_pubkey_order, update_order_to_initial_state};
use crate::lightning::LndConnector;
use crate::util::update_order_event;
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
    let job_older_orders_1m = Job::new_async("0 * * * * *", move |uuid, mut l| {
        Box::pin(async move {
            info!("Create a pool to connect to db");
            let pool = crate::db::connect().await;
            // Connect to relays
            let client = crate::util::connect_nostr().await;
            let keys = crate::util::get_keys();

            info!(
                "I run async every minute id {:?} - check older orders and mark them Expired",
                uuid
            );

            let older_orders_list = crate::db::find_order_by_date(pool.as_ref().unwrap()).await;

            for order in older_orders_list.unwrap().iter() {
                println!("Uid {} - created at {}", order.id, order.created_at);
                // We update the order id with the new event_id
                let _res = crate::util::update_order_event(
                    pool.as_ref().unwrap(),
                    client.as_ref().unwrap(),
                    keys.as_ref().unwrap(),
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

    let job_remove_pending_orders = Job::new_async("0 * * * * *", move |uuid, mut l| {
        Box::pin(async move {
            info!("Create a pool to connect to db");
            let pool = crate::db::connect().await;
            // Connect to relays
            let client = crate::util::connect_nostr().await;
            let keys = crate::util::get_keys();
            let mut ln_client = LndConnector::new().await;


            info!("I run async every minute id {:?} - check for order to republish for late actions", uuid);

            let older_orders_list = crate::db::find_order_by_seconds(pool.as_ref().unwrap()).await;

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
                    // Reset buyer pubkey to none
                    edit_buyer_pubkey_order(pool.as_ref().unwrap(),
                         order.id,
                         None)
                         .await.unwrap();
                    update_order_to_initial_state(pool.as_ref().unwrap(),
                         order.id,
                         updated_order_amount,
                         updated_order_fee).await.unwrap();
                    update_order_event(pool.as_ref().unwrap(),
                    client.as_ref().unwrap(),
                    keys.as_ref().unwrap(),
                            Status::Pending,
                                &order,
                                 None)
                                .await.unwrap();
                    info!(
                        "Canceled order Id {} republishing order not received regular invoice in time",
                         order.id
                    );
                }

            }
            let next_tick = l.next_tick_for_job(uuid).await;
            match next_tick {
                Ok(Some(ts)) => info!("Next time for 15 minutes is {:?}", ts),
                _ => warn!("Could not get next tick for job"),
            }
        })
    })
    .unwrap();

    // Add the task to the scheduler
    sched.add(job_older_orders_1m).await?;
    sched.add(job_remove_pending_orders).await?;

    Ok(())
}
