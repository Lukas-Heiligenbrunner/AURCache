use crate::builder::docker::{
    create_build_container, establish_docker_connection, monitor_build_output, repull_image,
};
use crate::builder::env::job_timeout_from_env;
use crate::builder::logger::BuildLogger;
use crate::builder::types::BuildStates;
use crate::db::files::ActiveModel;
use crate::db::migration::JoinType;
use crate::db::prelude::{Files, PackagesFiles};
use crate::db::{builds, files, packages, packages_files};
use crate::repo::utils::try_remove_archive_file;
use anyhow::{anyhow, bail};
use bollard::container::{KillContainerOptions, WaitContainerOptions};
use log::{debug, info};
use rocket::futures::StreamExt;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, ModelTrait, QueryFilter,
    QuerySelect, RelationTrait, Set, TransactionTrait,
};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;
use tokio::time::timeout;

static BUILDER_IMAGE: &str = "ghcr.io/lukas-heiligenbrunner/aurcache-builder:latest";

pub(crate) async fn prepare_build(
    mut build_model: builds::ActiveModel,
    db: DatabaseConnection,
    mut package_model: packages::ActiveModel,
) -> anyhow::Result<(packages::ActiveModel, builds::ActiveModel, String)> {
    // set build status to building
    build_model.status = Set(Some(BuildStates::ACTIVE_BUILD));
    build_model.start_time = Set(Some(
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64,
    ));
    let build_model = build_model.save(&db).await?;

    // update status to building
    package_model.status = Set(BuildStates::ACTIVE_BUILD);
    package_model = package_model.update(&db).await?.into();

    let target_platform = format!("linux/{}", build_model.platform.clone().unwrap());

    #[cfg(target_arch = "aarch64")]
    if target_platform != "linux/arm64" {
        bail!("Unsupported host architecture aarch64 for cross-compile");
    }

    Ok((package_model, build_model, target_platform))
}

pub async fn build(
    build_model: builds::ActiveModel,
    db: &DatabaseConnection,
    package_model: packages::ActiveModel,
    job_containers: Arc<Mutex<HashMap<i32, String>>>,
    build_logger: BuildLogger,
) -> anyhow::Result<()> {
    debug!("Preparing build");
    let (package_model_am, build_model_am, target_platform) =
        prepare_build(build_model, db.clone(), package_model).await?;
    let package_model: packages::Model = package_model_am.try_into()?;
    let build_model: builds::Model = build_model_am.try_into()?;
    debug!("Build {}: Establish docker connection", build_model.id);
    let docker = establish_docker_connection().await?;

    debug!("Build #{}: Repull builder image", build_model.id);
    let image_id = repull_image(
        &docker,
        &build_logger,
        BUILDER_IMAGE,
        target_platform.clone(),
    )
    .await?;
    debug!(
        "Build #{}: Image pulled with id: {}",
        build_model.id, image_id
    );

    debug!("Build #{}: Creating build container", build_model.id);
    let (create_info, host_active_build_path) = create_build_container(
        &docker,
        build_model.id,
        package_model.name.clone(),
        target_platform,
        image_id,
        package_model.build_flags.split(";").collect(),
    )
    .await?;
    let id = create_info.id;
    debug!(
        "Build #{}: build container created with id: {}",
        build_model.id, id
    );

    let docker2 = docker.clone();
    let id2 = id.clone();
    let build_logger2 = build_logger.clone();
    // start listening to container before starting it
    tokio::spawn(async move {
        _ = monitor_build_output(&build_logger2, &docker2, id2.clone()).await;
    });

    // start build container
    debug!("Build #{}: starting build container", build_model.id);
    docker.start_container::<String>(&id, None).await?;

    // insert container id to container map
    job_containers
        .lock()
        .await
        .insert(build_model.id, id.clone());

    // monitor build output
    debug!(
        "Build #{}: awaiting build container to exit",
        build_model.id
    );
    let build_result = timeout(
        job_timeout_from_env(),
        docker
            .wait_container(
                &id,
                Some(WaitContainerOptions {
                    condition: "not-running",
                }),
            )
            .next(),
    )
    .await;

    debug!("Build container was removed");

    match build_result {
        Ok(v) => {
            let t = v.ok_or(anyhow!("Failed to get build result"))??;
            let exit_code = t.status_code;
            if exit_code != 0 {
                build_logger
                    .append(format!(
                        "Build #{} failed for package '{}', exit code: {}",
                        build_model.id, package_model.name, exit_code
                    ))
                    .await?;
                bail!("Build failed with exit code: {}", exit_code);
            }
        }
        // timeout branch
        Err(_) => {
            build_logger
                .append(format!(
                    "Build #{} timed out for package '{}'",
                    build_model.id, package_model.name
                ))
                .await?;
            // kill build container
            docker
                .kill_container(&id, Some(KillContainerOptions { signal: "SIGKILL" }))
                .await?;
        }
    }

    // move built tar.gz archives to host and repo-add
    debug!("Build {}: Move built packages to repo", build_model.id);
    move_and_add_pkgs(
        &build_logger,
        host_active_build_path.clone(),
        package_model.id,
        db,
        build_model.platform,
    )
    .await?;
    // remove active build dir
    debug!("Build {}: Remove shared build folder", build_model.id);
    fs::remove_dir(host_active_build_path)?;
    Ok(())
}

/// move built files from build container to host and add them to the repo
async fn move_and_add_pkgs(
    build_logger: &BuildLogger,
    host_build_path: PathBuf,
    pkg_id: i32,
    db: &DatabaseConnection,
    platform: String,
) -> anyhow::Result<()> {
    let archive_paths = fs::read_dir(host_build_path.clone())?.collect::<Vec<_>>();
    if archive_paths.is_empty() {
        bail!("No files found in build directory");
    }

    // remove old files from repo and from direcotry
    // remove files assosicated with package
    let old_files: Vec<(packages_files::Model, Option<files::Model>)> = PackagesFiles::find()
        .filter(packages_files::Column::PackageId.eq(pkg_id))
        .filter(files::Column::Platform.eq(platform.clone()))
        .join(JoinType::LeftJoin, packages_files::Relation::Files.def())
        .select_also(files::Entity)
        .all(db)
        .await?;

    build_logger
        .append(format!(
            "Copy {} files from build dir to repo\nDeleting {} old files\n",
            archive_paths.len(),
            old_files.len()
        ))
        .await?;

    let txn = db.begin().await?;
    for (pkg_file, file) in old_files {
        pkg_file.delete(&txn).await?;

        if let Some(file) = file {
            try_remove_archive_file(file, &txn).await?;
        }
    }

    for archive in archive_paths {
        let archive = archive?;
        let archive_name = archive.file_name().to_str().unwrap().to_string();
        let pkg_path = format!("./repo/{platform}/{archive_name}");
        fs::copy(archive.path(), pkg_path.clone())?;
        // remove old file from shared path
        fs::remove_file(archive.path())?;

        let file = match Files::find()
            .filter(files::Column::Filename.eq(archive_name.clone()))
            .one(&txn)
            .await?
        {
            None => {
                let file = files::ActiveModel {
                    filename: Set(archive_name.clone()),
                    platform: Set(platform.clone()),
                    ..Default::default()
                };
                file.save(&txn).await?
            }
            Some(file) => ActiveModel::from(file),
        };

        let package_file = packages_files::ActiveModel {
            file_id: Set(file.id.unwrap()),
            package_id: Set(pkg_id),
            ..Default::default()
        };
        package_file.save(&txn).await?;

        pacman_repo_utils::repo_add(
            pkg_path.as_str(),
            format!("./repo/{platform}/repo.db.tar.gz"),
            format!("./repo/{platform}/repo.files.tar.gz"),
        )?;
        info!("Successfully added '{}' to the repo archive", archive_name);
    }
    txn.commit().await?;
    Ok(())
}
