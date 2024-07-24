use crate::builder::logger::BuildLogger;
use crate::db::files::ActiveModel;
use crate::db::migration::JoinType;
use crate::db::prelude::{Builds, Files, PackagesFiles};
use crate::db::{builds, files, packages, packages_files};
use crate::repo::utils::{repo_add, try_remove_archive_file};
use anyhow::anyhow;
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
use tokio::task::JoinHandle;

pub(crate) async fn cancel_build(
    build_id: i32,
    job_handles: Arc<Mutex<HashMap<i32, JoinHandle<()>>>>,
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
            .as_secs() as u32,
    ));
    let _ = build.clone().update(&db).await;

    job_handles
        .lock()
        .await
        .remove(&build.id.clone().unwrap())
        .ok_or(anyhow!("No build found"))?
        .abort();
    Ok(())
}

pub(crate) async fn prepare_build(
    mut new_build: builds::ActiveModel,
    db: DatabaseConnection,
    mut package_model: packages::ActiveModel,
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
        db.clone(),
        build_logger.clone(),
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
                    .as_secs() as u32,
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
                    .as_secs() as u32,
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
    db: DatabaseConnection,
    build_logger: BuildLogger,
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

    let mut work_dir = env::current_dir()?;
    work_dir.push("builds");
    fs::create_dir_all(work_dir.clone())?;
    fs::set_permissions(work_dir.clone(), Permissions::from_mode(0o777))?;

    let host_build_path = env::var("BUILD_ARTIFACT_DIR").unwrap_or(work_dir.display().to_string());

    // create new docker container for current build
    let mountpoint = format!("{}:/var/cache/makepkg/pkg", host_build_path);
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
                .cmd(vec![
                    "paru",
                    "-Syu",
                    "--noconfirm",
                    "--noprogressbar",
                    "--color",
                    "never",
                    name.as_str(),
                ])
                .build(),
        )
        .await?;
    let id = create_info.id;
    docker.containers().get(&id).start().await?;

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

    move_and_add_pkgs(build_logger, work_dir, pkg_id, db).await?;
    Ok(())
}

/// move built files from build container to host and add them to the repo
async fn move_and_add_pkgs(
    build_logger: BuildLogger,
    work_dir: PathBuf,
    pkg_id: i32,
    db: DatabaseConnection,
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
        .all(&db)
        .await?;

    let txn = db.begin().await?;
    for (pkg_file, file) in old_files {
        pkg_file.delete(&txn).await?;

        if let Some(file) = file {
            try_remove_archive_file(file, &txn).await?;
        }
    }

    build_logger
        .append(format!(
            "Copy {} files from build dir to repo",
            archive_paths.len()
        ))
        .await?;
    for archive in archive_paths {
        let archive = archive?;
        let archive_name = archive.file_name().to_str().unwrap().to_string();
        fs::copy(archive.path(), format!("./repo/{archive_name}"))?;
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
        build_logger
            .append(format!(
                "Successfully added '{}' to the repo archive",
                archive_name
            ))
            .await?;
    }
    txn.commit().await?;
    Ok(())
}
