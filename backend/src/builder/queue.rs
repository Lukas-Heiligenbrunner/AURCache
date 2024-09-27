use crate::builder::build::build;
use crate::builder::logger::BuildLogger;
use crate::builder::types::BuildStates;
use crate::db::builds::ActiveModel;
use crate::db::{builds, packages};
use sea_orm::{ActiveModelTrait, DatabaseConnection, Set, TransactionTrait};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::{Mutex, Semaphore};

/// Queue a package for building
pub(crate) async fn queue_package(
    package_model: Box<packages::ActiveModel>,
    build_model: Box<ActiveModel>,
    db: DatabaseConnection,
    semaphore: Arc<Semaphore>,
    job_containers: Arc<Mutex<HashMap<i32, String>>>,
) -> anyhow::Result<()> {
    let permits = Arc::clone(&semaphore);

    // spawn new thread for each pkg build
    // todo add queue and build two packages in parallel
    tokio::spawn(async move {
        let _permit = permits.acquire().await.unwrap();
        start_build(*build_model, &db, *package_model, job_containers).await;
    });
    Ok(())
}

async fn start_build(
    mut build_model: builds::ActiveModel,
    db: &DatabaseConnection,
    mut package_model: packages::ActiveModel,
    job_containers: Arc<Mutex<HashMap<i32, String>>>,
) {
    let build_id = build_model.id.clone().unwrap();
    let build_logger = BuildLogger::new(build_id, db.clone());

    let build_result = build(
        build_model.clone(),
        db,
        package_model.clone(),
        job_containers.clone(),
        build_logger.clone(),
    )
    .await;

    let txn = db.begin().await.unwrap();
    build_model.end_time = Set(Some(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64,
    ));

    match build_result {
        Ok(_) => {
            // update package success status
            package_model.status = Set(BuildStates::SUCCESSFUL_BUILD);
            package_model.out_of_date = Set(false as i32);
            _ = package_model.update(&txn).await;

            build_model.status = Set(Some(BuildStates::SUCCESSFUL_BUILD));

            let _ = build_model.update(&txn).await;
            _ = build_logger
                .append("finished package build".to_string())
                .await;
        }
        Err(e) => {
            package_model.status = Set(BuildStates::FAILED_BUILD);
            _ = package_model.update(&txn).await;

            build_model.status = Set(Some(BuildStates::FAILED_BUILD));
            let _ = build_model.update(&txn).await;

            _ = build_logger.append(e.to_string()).await;
        }
    };
    txn.commit().await.unwrap();

    // remove build from container map
    _ = job_containers.lock().await.remove(&build_id);
}
