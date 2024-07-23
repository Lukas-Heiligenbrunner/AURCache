use crate::builder::builder::prepare_build;
use crate::db::builds::ActiveModel;
use crate::db::packages;
use sea_orm::{ActiveModelTrait, DatabaseConnection, Set};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, Semaphore};
use tokio::task::JoinHandle;

pub(crate) async fn queue_package(
    name: String,
    version: String,
    package_model: Box<packages::ActiveModel>,
    mut build_model: Box<ActiveModel>,
    db: DatabaseConnection,
    semaphore: Arc<Semaphore>,
    job_handles: Arc<Mutex<HashMap<i32, JoinHandle<()>>>>,
) -> anyhow::Result<()> {
    let permits = Arc::clone(&semaphore);
    let build_id = build_model.id.clone().unwrap();

    // spawn new thread for each pkg build
    // todo add queue and build two packages in parallel
    let handle = tokio::spawn(async move {
        let _permit = permits.acquire().await.unwrap();

        // set build status to building
        build_model.status = Set(Some(0));
        let build_model = build_model.save(&db).await.unwrap();

        let _ = prepare_build(build_model, db, *package_model, version, name).await;
    });
    job_handles.lock().await.insert(build_id, handle);
    Ok(())
}
