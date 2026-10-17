use crate::{Wake, sleep_until_next_fire};
use aurcache_activitylog::events::Event;
use aurcache_common::schedule::is_off;
use aurcache_utils::package::update::package_update_all_outdated;
use aurcache_utils::scheduled::{Job, schedule};
use aurcache_utils::services::Services;
use aurcache_utils::settings;
use std::time::Duration;
use tokio::task::JoinHandle;
use tracing::info;

/// How long a change to the schedule may take to be noticed.
const RECHECK: Duration = Duration::from_mins(5);

#[must_use]
pub fn start_auto_update_job(services: Services) -> JoinHandle<()> {
    tokio::spawn(async move {
        // What the log was last told about the schedule. Re-checked every
        // few minutes, and recording the same news each time would bury
        // everything else; a schedule that is fixed and broken again says so.
        let mut reported: Option<Event> = None;
        let mut report = |event: Event| {
            if reported.as_ref() != Some(&event) {
                services.activity.emit(event.clone());
                reported = Some(event);
            }
        };
        let job = || Job::AutoUpdate.name().to_string();
        loop {
            // Read on every turn: it is a setting, and may have changed.
            let interval =
                settings::get(&services.db, settings::key::AUTO_UPDATE_SCHEDULE, None).await;
            let expr = interval.value.filter(|expr| !is_off(expr));
            match expr.as_deref().map(|expr| schedule(Job::AutoUpdate, expr)) {
                // Off.
                None => tokio::time::sleep(RECHECK).await,
                Some(Err(e)) => {
                    report(Event::ScheduleInvalid {
                        job: job(),
                        error: e.to_string(),
                    });
                    tokio::time::sleep(RECHECK).await;
                }
                Some(Ok(read)) => {
                    if let (Some(written), Some(read_as)) = (expr.as_deref(), &read.rewritten) {
                        report(Event::ScheduleOutdated {
                            job: job(),
                            written: written.to_string(),
                            read_as: read_as.clone(),
                        });
                    }
                    match sleep_until_next_fire(&read.schedule, "update", RECHECK).await {
                        Wake::Fired => {
                            info!("Executing scheduled auto-update");
                            if let Err(e) = package_update_all_outdated(&services).await {
                                services.activity.emit(Event::UpdateQueueFailed {
                                    error: format!("{e:#}"),
                                });
                            }
                        }
                        Wake::Recheck => {}
                        Wake::Exhausted => {
                            report(Event::ScheduleInvalid {
                                job: job(),
                                error: "the schedule never fires again".to_string(),
                            });
                            tokio::time::sleep(RECHECK).await;
                        }
                    }
                }
            }
        }
    })
}
