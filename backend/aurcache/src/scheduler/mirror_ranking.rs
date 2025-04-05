use crate::builder::types::Action;
use chrono::Utc;
use cron::Schedule;
use log::{info, warn};
use pacman_mirrors::benchmark::Bench;
use pacman_mirrors::platforms::Platform;
use sea_orm::DatabaseConnection;
use std::env;
use std::str::FromStr;
use std::time::Duration;
use tokio::sync::broadcast::Sender;
use tokio::task::JoinHandle;

pub fn start_mirror_rank_job(
    _db: DatabaseConnection,
    _tx: Sender<Action>,
) -> anyhow::Result<JoinHandle<()>> {
    let cron_str = env::var("MIRROR_RANK_SCHEDULE")?;
    // This parses the string following this spec: https://www.quartz-scheduler.org/documentation/quartz-2.3.0/tutorials/crontrigger.html
    let schedule = Schedule::from_str(cron_str.as_str())?;

    Ok(tokio::spawn(async move {
        let mut upcoming = schedule.upcoming(Utc);
        loop {
            // Get the next occurrence from now
            if let Some(next_time) = upcoming.next() {
                let now = Utc::now();
                let duration = next_time
                    .signed_duration_since(now)
                    .to_std()
                    .expect("Time went backwards?");
                info!(
                    "Waiting for scheduled mirror ranking until {} ({} seconds)",
                    next_time,
                    duration.as_secs()
                );

                // Wait until the scheduled time
                tokio::time::sleep(duration).await;

                // Execute your scheduled code
                info!("Executing mirror ranking job at: {}", Utc::now());
                match pacman_mirrors::get_status(Platform::X86_64).await {
                    Ok(status) => {
                        let mut urls = status.urls;
                        let mirrors = urls.rank().await.unwrap();
                        let mirrorlist = urls.gen_mirrorlist(mirrors).unwrap();
                        println!("Mirrorlist:\n{}", mirrorlist);
                    }
                    Err(e) => {
                        warn!("Failed to get mirror list: {}", e);
                    }
                };
            } else {
                // If there is no upcoming occurrence (unlikely with cron), wait a default duration before retrying.
                warn!(
                    "Your defined cron-job doesn't have a future schedule: '{}'",
                    cron_str
                );
                tokio::time::sleep(Duration::from_secs(60 * 30)).await;
            }
        }
    }))
}
