use crate::builder::types::Action;
use crate::package::update::package_update_all_outdated;
use chrono::Utc;
use cron::Schedule;
use log::{info, warn};
use sea_orm::DatabaseConnection;
use std::env;
use std::str::FromStr;
use std::time::Duration;
use tokio::sync::broadcast::Sender;
use tokio::task::JoinHandle;

pub fn start_auto_update_job(
    db: DatabaseConnection,
    tx: Sender<Action>,
) -> anyhow::Result<JoinHandle<()>> {
    let cron_str = env::var("AUTO_UPDATE_SCHEDULE")?;
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
                    "Waiting for scheduled update until {} ({} seconds)",
                    next_time,
                    duration.as_secs()
                );

                // Wait until the scheduled time
                tokio::time::sleep(duration).await;

                // Execute your scheduled code
                info!("Executing scheduled job at: {}", Utc::now());
                match package_update_all_outdated(&db, &tx).await {
                    Ok(v) => {
                        info!("Triggered update of all outdated packages: {:?}", v);
                    }
                    Err(e) => {
                        warn!("Failed to trigger update of all outdated packages: {}", e);
                    }
                }
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
