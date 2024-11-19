use crate::builder::build::Builder;
use crate::db::{builds, packages};
use sea_orm::DatabaseConnection;
use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{Mutex, Semaphore};

/// Queue a package for building
pub(crate) async fn queue_package(
    package_model: Box<packages::Model>,
    build_model: Box<builds::Model>,
    db: DatabaseConnection,
    semaphore: Arc<Semaphore>,
    job_containers: Arc<Mutex<HashMap<i32, String>>>,
) -> anyhow::Result<()> {
    let permits = Arc::clone(&semaphore);

    // spawn new thread for each pkg build
    tokio::spawn(async move {
        let _permit = permits.acquire().await.unwrap();
        start_build(*build_model, &db, *package_model, job_containers).await;
    });
    Ok(())
}

async fn start_build(
    build_model: builds::Model,
    db: &DatabaseConnection,
    package_model: packages::Model,
    job_containers: Arc<Mutex<HashMap<i32, String>>>,
) {
    let mut builder = Builder::new(db.clone(), job_containers, package_model, build_model)
        .await
        .unwrap();
    let result = builder.build().await;
    builder.post_build(result).await.unwrap();
}
