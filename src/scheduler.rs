use anyhow::Ok;
use tokio_cron_scheduler::{Job, JobScheduler};
use tracing::{Level, info, error};
use tracing_subscriber::FmtSubscriber;


pub async fn cron_scheduler() -> Result<(), anyhow::Error>{
    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::TRACE)
        .finish();
    tracing::subscriber::set_global_default(subscriber).expect("Setting default subscriber failed");
    let sched = JobScheduler::new().await;
    let sched = sched.unwrap();

    let mut five_s_job = Job::new("1/5 * * * * *", |uuid, _l| {
        info!(
            "{:?} I run every 5 seconds id {:?}",
            chrono::Utc::now(),
            uuid
        );
    })
    .unwrap();

    // Adding a job notification without it being added to the scheduler will automatically add it to
    // the job store, but with stopped marking
    five_s_job
        .on_removed_notification_add(
            &sched,
            Box::new(|job_id, notification_id, type_of_notification| {
                Box::pin(async move {
                    info!(
                        "5s Job {:?} was removed, notification {:?} ran ({:?})",
                        job_id, notification_id, type_of_notification
                    );
                })
            }),
        )
        .await?;
    let five_s_job_guid = five_s_job.guid();
    sched.add(five_s_job).await?;

    let mut ten_s_job = Job::new("1/10 * * * * *", |uuid, _l| {
        info!(
            "{:?} I run every 10 seconds id {:?}",
            chrono::Utc::now(),
            uuid
        );
    })
    .unwrap();

    // Adding a job notification without it being added to the scheduler will automatically add it to
    // the job store, but with stopped marking
    ten_s_job
        .on_removed_notification_add(
            &sched,
            Box::new(|job_id, notification_id, type_of_notification| {
                Box::pin(async move {
                    info!(
                        "10s Job {:?} was removed, notification {:?} ran ({:?})",
                        job_id, notification_id, type_of_notification
                    );
                })
            }),
        )
        .await?;
    let ten_s_job_guid = ten_s_job.guid();
    sched.add(ten_s_job).await?;

    let start = sched.start().await;
    if start.is_err() {
        error!("Error starting scheduler");
        return Ok(());
    }

    tokio::time::sleep(core::time::Duration::from_secs(60)).await;
    
    Ok(())
}