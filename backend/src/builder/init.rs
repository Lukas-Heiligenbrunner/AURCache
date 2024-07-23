use crate::builder::builder::cancel_build;
use crate::builder::queue::queue_package;
use crate::builder::types::Action;
use sea_orm::DatabaseConnection;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::broadcast::Sender;
use tokio::sync::{Mutex, Semaphore};
use tokio::task::JoinHandle;

pub fn init_build_queue(db: DatabaseConnection, tx: Sender<Action>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let semaphore = Arc::new(Semaphore::new(1));
        let job_handles: Arc<Mutex<HashMap<i32, JoinHandle<_>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        loop {
            if let Ok(_result) = tx.subscribe().recv().await {
                match _result {
                    // add a package to parallel build
                    Action::Build(name, version, url, version_model, build_model) => {
                        let _ = queue_package(
                            name,
                            version,
                            version_model,
                            build_model,
                            db.clone(),
                            semaphore.clone(),
                            job_handles.clone(),
                        )
                        .await;
                    }
                    Action::Cancel(build_id) => {
                        let _ = cancel_build(build_id, job_handles.clone(), db.clone()).await;
                    }
                }
            }
        }
    })
}
