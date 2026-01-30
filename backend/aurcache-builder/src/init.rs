use crate::cancel::cancel_build;
use crate::queue::queue_package;
use aurcache_types::builder::Action;
use aurcache_types::settings::{ApplicationSettings, Setting, SettingsEntry};
use aurcache_utils::settings::general::SettingsTraits;
use sea_orm::DatabaseConnection;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::broadcast::Sender;
use tokio::sync::{Mutex, Semaphore};
use tokio::task::JoinHandle;

#[must_use]
pub fn init_build_queue(db: DatabaseConnection, tx: Sender<Action>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut concurrent_builds = get_max_concurrent_builds(&db).await;
        let semaphore = Arc::new(Semaphore::new(concurrent_builds));
        let job_containers: Arc<Mutex<HashMap<i32, String>>> = Arc::new(Mutex::new(HashMap::new()));

        loop {
            // Adjust semaphore permits dynamically
            let new_max = get_max_concurrent_builds(&db).await;
            if new_max != concurrent_builds {
                if new_max > concurrent_builds {
                    semaphore.add_permits(new_max - concurrent_builds);
                } else {
                    semaphore.forget_permits(concurrent_builds - new_max);
                }
                concurrent_builds = new_max;
            }

            if let Ok(_result) = tx.subscribe().recv().await {
                match _result {
                    // add a package to parallel build
                    Action::Build(package_model, build_model) => {
                        let _ = queue_package(
                            package_model,
                            build_model,
                            db.clone(),
                            semaphore.clone(),
                            job_containers.clone(),
                        )
                        .await;
                    }
                    Action::Cancel(build_id) => {
                        let _ = cancel_build(build_id, job_containers.clone(), db.clone()).await;
                    }
                }
            }
        }
    })
}

async fn get_max_concurrent_builds(db: &DatabaseConnection) -> usize {
    let max_concurrent_builds: SettingsEntry<u32> =
        ApplicationSettings::get(Setting::MaxConcurrentBuilds, None, db).await;
    max_concurrent_builds.value as usize
}
