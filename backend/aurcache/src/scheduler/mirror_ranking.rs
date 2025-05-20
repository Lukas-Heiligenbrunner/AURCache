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
use tokio::fs;
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
                match update_mirrorlist().await {
                    Ok(_) => {
                        info!("Mirror ranking finished");
                    }
                    Err(e) => {
                        warn!("Mirror ranking failed: {}", e);
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

async fn update_mirrorlist() -> anyhow::Result<()> {
    info!("Executing mirror ranking job at: {}", Utc::now());
    match pacman_mirrors::get_status(Platform::X86_64).await {
        Ok(status) => {
            let mut urls = status.urls;
            info!("Ranking mirrorlist");
            let mirrors = urls.rank().await?;
            let mirrorlist = urls.gen_mirrorlist(mirrors)?;

            let mirrorlist_path = get_mirrorlist_path();
            fs::write(mirrorlist_path.as_str(), mirrorlist).await?;
            info!("Wrote mirrorlist to {}", mirrorlist_path);
        }
        Err(e) => {
            warn!("Failed to get mirror list: {}", e);
        }
    };
    Ok(())
}

pub fn get_mirrorlist_path() -> String {
    let mirrorlist_path = option_env!("MIRRORLIST_PATH_X86_64").unwrap_or_else(|| {
        if std::fs::metadata("./config").is_err() {
            std::fs::create_dir("./config").expect(
                "Failed to create config directory. Maybe container directory is not writeable?",
            );
            info!("Created default MIRRORLIST_PATH_X86_64: ./config");
        }
        "./config/mirrorlist_x86_64"
    });
    mirrorlist_path.to_string()
}
