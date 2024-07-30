use crate::builder::logger::BuildLogger;
use crate::db::files::ActiveModel;
use crate::db::migration::JoinType;
use crate::db::prelude::{Builds, Files, PackagesFiles};
use crate::db::{builds, files, packages, packages_files};
use crate::repo::utils::{repo_add, try_remove_archive_file};
use anyhow::anyhow;
use log::info;
use rocket::futures::StreamExt;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, ModelTrait, QueryFilter,
    QuerySelect, RelationTrait, Set, TransactionTrait,
};
use shiplift::tty::TtyChunk;
use shiplift::{ContainerOptions, Docker, LogsOptions, PullOptions};
use std::collections::HashMap;
use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use std::{env, fs};
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

    let docker = Docker::new();
    docker.containers().get(container_id).stop(None).await?;

    job_containers
        .lock()
        .await
        .remove(&build_id)
        .ok_or(anyhow!(
            "Failed to remove build container from active build map"
        ))?;
    Ok(())
}

pub(crate) async fn prepare_build(
    mut new_build: builds::ActiveModel,
    db: DatabaseConnection,
    mut package_model: packages::ActiveModel,
    job_containers: Arc<Mutex<HashMap<i32, String>>>,
) -> anyhow::Result<()> {
    let build_id = new_build.id.clone().unwrap();
    let build_logger = BuildLogger::new(build_id, db.clone());

    // update status to building
    package_model.status = Set(0);
    package_model = package_model.update(&db).await?.into();

    let package_name = package_model.name.clone().unwrap();
    let package_id = package_model.id.clone().unwrap();

    match build(
        package_name,
        build_id,
        package_id,
        &db,
        build_logger.clone(),
        job_containers,
    )
    .await
    {
        Ok(_) => {
            // update package success status
            package_model.status = Set(1);
            package_model.out_of_date = Set(false as i32);
            package_model.update(&db).await?;

            new_build.status = Set(Some(1));
            new_build.end_time = Set(Some(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs() as i64,
            ));
            _ = new_build.update(&db).await;
            build_logger
                .append("finished package build".to_string())
                .await?;
        }
        Err(e) => {
            package_model.status = Set(2);
            package_model.update(&db).await?;

            new_build.status = Set(Some(2));
            new_build.end_time = Set(Some(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs() as i64,
            ));
            let _ = new_build.update(&db).await;

            build_logger.append(e.to_string()).await?;
        }
    };
    Ok(())
}

pub async fn build(
    name: String,
    build_id: i32,
    pkg_id: i32,
    db: &DatabaseConnection,
    build_logger: BuildLogger,
    job_containers: Arc<Mutex<HashMap<i32, String>>>,
) -> anyhow::Result<()> {
    let docker = Docker::new();

    match docker.ping().await {
        Ok(_) => {}
        Err(e) => return Err(anyhow!("Connection to Docker Socket failed: {}", e)),
    }

    // repull image to make sure it's up to date
    let mut stream = docker.images().pull(
        &PullOptions::builder()
            .image("docker.io/greyltc/archlinux-aur:paru")
            .build(),
    );

    while let Some(pull_result) = stream.next().await {
        match pull_result {
            Ok(output) => build_logger.append(format!("{:?}", output)).await?,
            Err(e) => build_logger.append(format!("{}", e)).await?,
        }
    }

    // create builds dir
    let mut host_build_path_base = env::current_dir()?;
    host_build_path_base.push("builds");
    fs::create_dir_all(host_build_path_base.clone())?;
    fs::set_permissions(host_build_path_base.clone(), Permissions::from_mode(0o777))?;

    // use either docker volume or base dir as docker host mount path
    let host_build_path_docker =
        env::var("BUILD_ARTIFACT_DIR").unwrap_or(host_build_path_base.display().to_string());

    // create current build dir
    let mut host_active_build_path = host_build_path_base.clone();
    host_active_build_path.push(name.clone());
    fs::create_dir_all(host_active_build_path.clone())?;
    fs::set_permissions(
        host_active_build_path.clone(),
        Permissions::from_mode(0o777),
    )?;

    // create new docker container for current build
    let build_dir_base = "/var/cache/makepkg/pkg";
    let mountpoint = format!("{}:{}", host_build_path_docker, build_dir_base);
    let makepkg_config = format!(
        "\
MAKEFLAGS=-j$(nproc)
PKGDEST={}/{}",
        build_dir_base, name
    );
    let makepkg_config_path = "/var/ab/.config/pacman/makepkg.conf";
    let cmd = format!(
        "cat <<EOF > {}\n{}\nEOF\nparu -Syu --noconfirm --noprogressbar --color never {}",
        makepkg_config_path, makepkg_config, name
    );
    let create_info = docker
        .containers()
        .create(
            &ContainerOptions::builder("docker.io/greyltc/archlinux-aur:paru")
                .volumes(vec![mountpoint.as_str()])
                .attach_stdout(true)
                .attach_stderr(true)
                .auto_remove(true)
                .user("ab")
                .name(format!("aurcache_build_{}_{}", name, build_id).as_str())
                .cmd(vec!["sh", "-c", cmd.as_str()])
                .build(),
        )
        .await?;
    let id = create_info.id;
    docker.containers().get(&id).start().await?;

    // insert container id to container map
    job_containers.lock().await.insert(build_id, id.clone());

    let mut logs_stream = docker.containers().get(id.clone()).logs(
        &LogsOptions::builder()
            .follow(true)
            .stdout(true)
            .stderr(true)
            .build(),
    );

    while let Some(log_result) = logs_stream.next().await {
        match log_result {
            Ok(chunk) => match chunk {
                TtyChunk::StdIn(_) => unreachable!(),
                TtyChunk::StdOut(bytes) => {
                    build_logger
                        .append(String::from_utf8(bytes).unwrap())
                        .await?
                }
                TtyChunk::StdErr(bytes) => {
                    build_logger
                        .append(String::from_utf8(bytes).unwrap())
                        .await?
                }
            },
            Err(e) => build_logger.append(e.to_string()).await?,
        }
    }

    job_containers
        .lock()
        .await
        .remove(&build_id)
        .ok_or(anyhow!(
            "Failed to remove build container from active builds map"
        ))?;

    // move built tar.gz archives to host and repo-add
    move_and_add_pkgs(build_logger, host_active_build_path.clone(), pkg_id, db).await?;
    // remove active build dir
    fs::remove_dir(host_active_build_path)?;
    Ok(())
}

/// move built files from build container to host and add them to the repo
async fn move_and_add_pkgs(
    build_logger: BuildLogger,
    work_dir: PathBuf,
    pkg_id: i32,
    db: &DatabaseConnection,
) -> anyhow::Result<()> {
    let archive_paths = fs::read_dir(work_dir.clone())?.collect::<Vec<_>>();
    if archive_paths.is_empty() {
        return Err(anyhow!("No files found in build directory"));
    }

    // remove old files from repo and from direcotry
    // remove files assosicated with package
    let old_files: Vec<(packages_files::Model, Option<files::Model>)> = PackagesFiles::find()
        .filter(packages_files::Column::PackageId.eq(pkg_id))
        .join(JoinType::LeftJoin, packages_files::Relation::Files.def())
        .select_also(files::Entity)
        .all(db)
        .await?;

    build_logger
        .append(format!(
            "Copy {} files from build dir to repo\nDeleting {} old files",
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
        fs::copy(archive.path(), format!("./repo/{archive_name}"))?;
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

        repo_add(archive_name.clone())?;
        info!("Successfully added '{}' to the repo archive", archive_name);
    }
    txn.commit().await?;
    Ok(())
}
