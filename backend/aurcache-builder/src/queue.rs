use crate::build::Builder;
use aurcache_db::helpers::active_value_ext::ActiveValueExt;
use aurcache_db::{builds, packages};
use aurcache_types::builder::Action;
use sea_orm::DatabaseConnection;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::broadcast::Sender;
use tokio::sync::{Mutex, Semaphore};
use tracing::error;

/// Queue a package for building
pub(crate) async fn queue_package(
    package_model: Box<packages::Model>,
    build_model: Box<builds::Model>,
    db: DatabaseConnection,
    semaphore: Arc<Semaphore>,
    job_containers: Arc<Mutex<HashMap<i32, String>>>,
    action_tx: Sender<Action>,
) -> anyhow::Result<()> {
    let permits = Arc::clone(&semaphore);

    // spawn new thread for each pkg build
    tokio::spawn(async move {
        let _permit = permits.acquire().await.unwrap();
        start_build(*build_model, &db, *package_model, job_containers, action_tx).await;
    });
    Ok(())
}

async fn start_build(
    build_model: builds::Model,
    db: &DatabaseConnection,
    package_model: packages::Model,
    job_containers: Arc<Mutex<HashMap<i32, String>>>,
    action_tx: Sender<Action>,
) {
    let mut builder = match Builder::new(
        db.clone(),
        job_containers,
        package_model,
        build_model,
        action_tx,
    )
    .await
    {
        Ok(v) => v,
        Err(e) => {
            error!("Error while creating builder: {e}");
            return;
        }
    };
    let result = builder.build().await;
    if let Err(e) = builder.post_build(result).await {
        error!(
            "Error in post-build of build #{}: {}",
            builder.build_model.id.get().unwrap(),
            e
        );
    }
}
