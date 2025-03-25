use crate::builder::types::Action;
use chrono::Utc;
use cron::Schedule;
use log::{info, warn};
use sea_orm::DatabaseConnection;
use std::env;
use std::str::FromStr;
use std::time::Duration;
use tokio::sync::broadcast::Sender;
use tokio::task::JoinHandle;
use pacman_mirrors::benchmark::Rank;

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
                match pacman_mirrors::get_status().await {
                    Ok(mut status) => {
                        let mirrors = status.urls.rank().unwrap();

                        println!(
                            r#"##
## Arch Linux repository mirrorlist
## Created by arch_mirrors
## Generated on {}
##
"#,
                            Utc::now().date_naive()
                        );

                        for mirror in mirrors {
                            println!("## {}", mirror.country.kind);
                            println!("#Server = {}$repo/os/$arch", mirror.url)
                        }
                    },
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
