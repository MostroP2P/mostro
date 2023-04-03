use std::error::Error;
use sqlx::SqlitePool;
use tokio_cron_scheduler::{Job, JobScheduler};
use tracing::{info, Level, warn};
use tracing_subscriber::FmtSubscriber;
use anyhow::Result;

pub fn scheduler_mostro() {
    let handle = std::thread::Builder::new()
        .name("schedule thread".to_string())
        .spawn(move || {
            // tokio::runtime::Builder::new_current_thread()    <- This hangs
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("build runtime failed")
                .block_on(start())
                .expect("TODO: panic message");
        })
        .expect("spawn thread failed");
    handle.join().expect("join failed");
}


async fn start() -> Result<(), Box<dyn Error>> {
    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::TRACE)
        .finish();
    tracing::subscriber::set_global_default(subscriber).expect("Setting default subscriber failed");
    info!("Creating scheduler");
    let sched = JobScheduler::new().await?;
    info!("Run example");
    cron_scheduler(sched).await;
    Ok(())
}


pub async fn cron_scheduler(mut sched: JobScheduler) -> Result<(), anyhow::Error>{

    let mut four_s_job_async = Job::new_async("1/4 * * * * *", move |uuid, mut l| {
        Box::pin(async move {
            info!("Create a pool to connect to db ");
            let pool = crate::db::connect().await;
            info!("I run async every 4 seconds id {:?}", uuid);
            let time = crate::db::find_order_by_date(&pool.unwrap()).await;

            for el in time.unwrap().iter(){
                println!("Uid {} - created {}",el.id,el.created_at)
            }

            let next_tick = l.next_tick_for_job(uuid).await;
            match next_tick {
                Ok(Some(ts)) => info!("Next time for 4s is {:?}", ts),
                _ => warn!("Could not get next tick for 4s job"),
            }
        })
    })
    .unwrap();

    let four_s_job_async_clone = four_s_job_async.clone();
    let js = sched.clone();
    info!("4s job id {:?}", four_s_job_async.guid());
    four_s_job_async.on_start_notification_add(&sched, Box::new(move |job_id, notification_id, type_of_notification| {
        let four_s_job_async_clone = four_s_job_async_clone.clone();
        let js = js.clone();
        Box::pin(async move {
            info!("4s Job {:?} ran on start notification {:?} ({:?})", job_id, notification_id, type_of_notification);
            info!("This should only run once since we're going to remove this notification immediately.");
            info!("Removed? {:?}", four_s_job_async_clone.on_start_notification_remove(&js, &notification_id).await);
        })
    })).await?;

    four_s_job_async
        .on_done_notification_add(
            &sched,
            Box::new(|job_id, notification_id, type_of_notification| {
                Box::pin(async move {
                    info!(
                        "4s Job {:?} completed and ran notification {:?} ({:?})",
                        job_id, notification_id, type_of_notification
                    );
                })
            }),
        )
        .await?;

    let four_s_job_guid = four_s_job_async.guid();
    sched.add(four_s_job_async).await?;

    let start = sched.start().await;
    if start.is_err() {
        return Ok(());
    }

    tokio::time::sleep(core::time::Duration::from_secs(50)).await;

    Ok(())   
  
}