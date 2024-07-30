use crate::builder::build::prepare_build;
use crate::db::builds::ActiveModel;
use crate::db::packages;
use sea_orm::{ActiveModelTrait, DatabaseConnection, Set};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::{Mutex, Semaphore};

pub(crate) async fn queue_package(
    package_model: Box<packages::ActiveModel>,
    mut build_model: Box<ActiveModel>,
    db: DatabaseConnection,
    semaphore: Arc<Semaphore>,
    job_containers: Arc<Mutex<HashMap<i32, String>>>,
) -> anyhow::Result<()> {
    let permits = Arc::clone(&semaphore);

    // spawn new thread for each pkg build
    // todo add queue and build two packages in parallel
    tokio::spawn(async move {
        let _permit = permits.acquire().await.unwrap();

        // set build status to building
        build_model.status = Set(Some(0));
        build_model.start_time = Set(Some(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64,
        ));
        let build_model = build_model.save(&db).await.unwrap();

        let _ = prepare_build(build_model, db, *package_model, job_containers).await;
    });
    Ok(())
}
