use crate::build::Builder;
use aurcache_db::helpers::active_value_ext::ActiveValueExt;
use aurcache_db::prelude::Builds;
use aurcache_db::{builds, packages};
use aurcache_deps::AurClient;
use aurcache_types::builder::Action;
use aurcache_types::builder::BuildStates;
use aurcache_utils::snapshot::SnapshotStore;
use sea_orm::ActiveModelTrait;
use sea_orm::ActiveValue::Set;
use sea_orm::DatabaseConnection;
use sea_orm::EntityTrait;
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
    store: Arc<SnapshotStore>,
) -> anyhow::Result<()> {
    let permits = Arc::clone(&semaphore);
    let client = AurClient::new();

    // spawn new thread for each pkg build
    tokio::spawn(async move {
        let _permit = permits.acquire().await.unwrap();
        start_build(
            *build_model,
            &db,
            *package_model,
            job_containers,
            action_tx,
            client,
            store,
        )
        .await;
    });
    Ok(())
}

async fn start_build(
    build_model: builds::Model,
    db: &DatabaseConnection,
    package_model: packages::Model,
    job_containers: Arc<Mutex<HashMap<i32, String>>>,
    action_tx: Sender<Action>,
    client: AurClient,
    store: Arc<SnapshotStore>,
) {
    let build_id = build_model.id;
    let mut builder = match Builder::new(
        db.clone(),
        job_containers,
        package_model,
        build_model,
        action_tx,
        client,
        store,
    )
    .await
    {
        Ok(v) => v,
        Err(e) => {
            error!("Error while creating builder: {e}");
            // Mark the build as failed so it doesn't stay stuck in ENQUEUED.
            if let Ok(Some(b)) = Builds::find_by_id(build_id).one(db).await {
                let mut build_active: builds::ActiveModel = b.into();
                build_active.status = Set(Some(BuildStates::FAILED_BUILD));
                build_active.end_time = Set(Some(
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs() as i64,
                ));
                let _ = build_active.save(db).await;
            }
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
