use crate::db::builds;
use crate::db::prelude::Builds;
use anyhow::anyhow;
use bollard::Docker;
use bollard::query_parameters::RemoveContainerOptions;
use sea_orm::{ActiveModelTrait, DatabaseConnection, EntityTrait, Set};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;

pub(crate) async fn cancel_build(
    build_id: i32,
    job_containers: Arc<Mutex<HashMap<i32, String>>>,
    db: DatabaseConnection,
) -> anyhow::Result<()> {
    let build = Builds::find_by_id(build_id)
        .one(&db)
        .await?
        .ok_or(anyhow!("No build found"))?;

    let mut build: builds::ActiveModel = build.into();
    build.status = Set(Some(4));
    build.end_time = Set(Some(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64,
    ));
    let _ = build.clone().update(&db).await;

    let container_id = job_containers
        .lock()
        .await
        .get(&build_id)
        .ok_or(anyhow!("Build container not found"))?
        .clone();

    let docker = Docker::connect_with_unix_defaults()?;
    docker
        .remove_container(
            &container_id,
            Some(RemoveContainerOptions {
                force: true,
                ..Default::default()
            }),
        )
        .await?;

    job_containers
        .lock()
        .await
        .remove(&build_id)
        .ok_or(anyhow!(
            "Failed to remove build container from active build map"
        ))?;
    Ok(())
}
