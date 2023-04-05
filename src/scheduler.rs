use anyhow::Result;
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

            info!("I run async every minute id {:?}", uuid);

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
                )
                .await;
                //Send dm here???
            }
            let next_tick = l.next_tick_for_job(uuid).await;
            match next_tick {
                Ok(Some(ts)) => info!("Next time for 1 minute is {:?}", ts),
                _ => warn!("Could not get next tick for 4s job"),
            }
        })
    })
    .unwrap();

    // Add the task to the scheduler
    sched.add(job_older_orders_1m).await?;

    Ok(())
}
