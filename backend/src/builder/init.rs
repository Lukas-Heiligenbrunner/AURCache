use crate::builder::cancel::cancel_build;
use crate::builder::queue::queue_package;
use crate::builder::types::Action;
use sea_orm::DatabaseConnection;
use std::collections::HashMap;
use std::env;
use std::sync::Arc;
use tokio::sync::broadcast::Sender;
use tokio::sync::{Mutex, Semaphore};
use tokio::task::JoinHandle;

pub fn init_build_queue(db: DatabaseConnection, tx: Sender<Action>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let semaphore = new_semaphore();
        let job_containers: Arc<Mutex<HashMap<i32, String>>> = Arc::new(Mutex::new(HashMap::new()));

        loop {
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

fn new_semaphore() -> Arc<Semaphore> {
    let max_concurrent_builds = env::var("MAX_CONCURRENT_BUILDS")
        .ok()
        .and_then(|x| x.parse::<usize>().ok())
        .unwrap_or(1);
    Arc::new(Semaphore::new(max_concurrent_builds))
}
