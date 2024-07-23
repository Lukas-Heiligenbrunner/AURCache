use crate::builder::logger::spawn_log_appender;
use crate::db::prelude::{Builds, Packages};
use crate::db::{builds, packages, versions};
use crate::repo::repo::repo_add;
use anyhow::anyhow;
use rocket::futures::StreamExt;
use sea_orm::{ActiveModelTrait, DatabaseConnection, EntityTrait, Set};
use shiplift::tty::TtyChunk;
use shiplift::{ContainerOptions, Docker, LogsOptions, PullOptions};
use std::collections::HashMap;
use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use std::{env, fs};
use tokio::sync::broadcast::Sender;
use tokio::sync::{broadcast, Mutex};
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
    mut version_model: versions::ActiveModel,
    version: String,
    name: String,
) -> anyhow::Result<()> {
    let (tx, rx) = broadcast::channel::<String>(3);
    spawn_log_appender(db.clone(), new_build.clone(), rx);

    let package_id = version_model.package_id.clone().unwrap();
    let mut pkg: packages::ActiveModel = Packages::find_by_id(package_id)
        .one(&db)
        .await?
        .ok_or(anyhow!("no package with id {package_id} found"))?
        .into();

    // update status to building
    pkg.status = Set(0);
    pkg = pkg.update(&db).await?.into();

    let build_id = new_build.id.clone().unwrap();

    match build(version, name, tx.clone(), build_id).await {
        Ok(pkg_file_name) => {
            _ = tx.send("successfully built package".to_string());

            // update package success status
            pkg.status = Set(1);
            pkg.latest_version_id = Set(Some(version_model.id.clone().unwrap()));
            pkg.out_of_date = Set(false as i32);
            pkg.update(&db).await?;

            version_model.file_name = Set(Some(pkg_file_name));
            let _ = version_model.update(&db).await;

            new_build.status = Set(Some(1));
            new_build.end_time = Set(Some(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs() as u32,
            ));
            let _ = new_build.update(&db).await;
        }
        Err(e) => {
            pkg.status = Set(2);
            pkg.latest_version_id = Set(Some(version_model.id.clone().unwrap()));
            pkg.update(&db).await?;

            let _ = version_model.update(&db).await;

            new_build.status = Set(Some(2));
            new_build.end_time = Set(Some(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs() as u32,
            ));
            let _ = new_build.update(&db).await;

            _ = tx.send(e.to_string());
        }
    };
    Ok(())
}

pub async fn build(
    version: String,
    name: String,
    tx: Sender<String>,
    build_id: i32,
) -> anyhow::Result<String> {
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
            Ok(output) => _ = tx.send(format!("{:?}", output)),
            Err(e) => _ = tx.send(format!("{}", e)),
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
                TtyChunk::StdOut(bytes) => _ = tx.send(String::from_utf8(bytes).unwrap()),
                TtyChunk::StdErr(bytes) => _ = tx.send(String::from_utf8(bytes).unwrap()),
            },
            Err(e) => _ = tx.send(e.to_string()),
        }
    }

    let archive_paths = fs::read_dir(work_dir.clone())?.collect::<Vec<_>>();
    if archive_paths.is_empty() {
        return Err(anyhow!("No files found in build directory"));
    }

    _ = tx.send(format!(
        "Copy {} files from build dir to repo",
        archive_paths.len()
    ));
    for archive in archive_paths {
        let archive = archive?;
        let archive_name = archive.file_name().to_str().unwrap().to_string();
        fs::copy(archive.path(), format!("./repo/{archive_name}"))?;
        fs::remove_file(archive.path())?;

        repo_add(archive_name.clone())?;
        _ = tx.send(format!(
            "Successfully added '{}' to the repo archive",
            archive_name
        ));
    }

    Ok(name)
}
