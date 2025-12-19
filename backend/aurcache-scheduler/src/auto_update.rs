use aurcache_types::builder::Action;
use aurcache_types::settings::{ApplicationSettings, Setting, SettingsEntry};
use aurcache_utils::package::update::package_update_all_outdated;
use aurcache_utils::settings::general::SettingsTraits;
use chrono::Utc;
use cron::Schedule;
use sea_orm::DatabaseConnection;
use std::str::FromStr;
use std::time::Duration;
use tokio::sync::broadcast::Sender;
use tokio::task::JoinHandle;
use tracing::{info, warn};

pub fn start_auto_update_job(
    db: DatabaseConnection,
    tx: Sender<Action>,
) -> anyhow::Result<JoinHandle<()>> {
    Ok(tokio::spawn(async move {
        loop {
            // check everytime in loop since it may change per user setting
            let interval: SettingsEntry<Option<String>> =
                ApplicationSettings::get(Setting::AutoUpdateInterval, None, &db).await;
            match interval.value.as_deref().map(Schedule::from_str) {
                None => {
                    // Auto update disabled
                    tokio::time::sleep(Duration::from_hours(1)).await;
                }
                Some(Err(e)) => {
                    warn!("Invalid cron expression: {e} -- Retry in 15min");
                    tokio::time::sleep(Duration::from_mins(15)).await;
                }
                Some(Ok(schedule)) => {
                    let mut upcoming = schedule.upcoming(Utc);

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

                        tokio::time::sleep(duration).await;

                        info!("Executing scheduled job at: {}", Utc::now());
                        if let Err(e) = package_update_all_outdated(&db, &tx).await {
                            warn!("Failed to trigger update of all outdated packages: {e}");
                        }
                    } else {
                        warn!("Your defined cron-job doesn't have a future schedule");
                        tokio::time::sleep(Duration::from_mins(30)).await;
                    }
                }
            }
        }
    }))
}
