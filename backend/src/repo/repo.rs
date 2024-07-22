use crate::db::prelude::Packages;
use crate::db::prelude::Versions;
use crate::db::versions;
use anyhow::anyhow;
use rocket::futures::StreamExt;
use sea_orm::{
    ColumnTrait, DatabaseConnection, DatabaseTransaction, EntityTrait, ModelTrait, QueryFilter,
    TransactionTrait,
};
use shiplift::tty::TtyChunk;
use shiplift::{ContainerOptions, Docker, LogsOptions, PullOptions};
use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;
use std::{env, fs};
use tokio::sync::broadcast::Sender;

static REPO_NAME: &str = "repo";

pub async fn add_pkg(
    version: String,
    name: String,
    tx: Sender<String>,
    build_id: i32,
) -> anyhow::Result<String> {
    let docker = Docker::new();

    // repull image to make sure it's up to date
    let mut stream = docker.images().pull(
        &PullOptions::builder()
            .image("docker.io/greyltc/archlinux-aur:paru")
            .build(),
    );

    while let Some(pull_result) = stream.next().await {
        match pull_result {
            Ok(output) => {
                println!("{:?}", output);
                _ = tx.send(format!("{:?}", output));
            }
            Err(e) => {
                eprintln!("Error: {}", e);
                _ = tx.send(format!("{}", e));
            }
        }
    }

    let mut work_dir = env::current_dir()?;
    work_dir.push("builds");
    fs::create_dir_all(work_dir.clone())?;
    fs::set_permissions(work_dir.clone(), Permissions::from_mode(0o777))?;

    let host_build_path = env::var("BUILD_CONTAINER_DIR").unwrap_or(work_dir.display().to_string());

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

    for archive in archive_paths {
        let archive = archive?;
        let archive_name = archive.file_name().to_str().unwrap().to_string();
        // todo force overwrite if file already exists
        fs::copy(archive.path(), format!("./repo/{archive_name}"))?;
        fs::remove_file(archive.path())?;

        repo_add(archive_name)?;
    }

    fs::remove_dir(work_dir)?;

    Ok(name)
}

pub async fn remove_version(db: &DatabaseConnection, version_id: i32) -> anyhow::Result<()> {
    let txn = db.begin().await?;

    let version = Versions::find()
        .filter(versions::Column::PackageId.eq(version_id))
        .one(&txn)
        .await?;
    if let Some(version) = version {
        rem_ver(&txn, version).await?;
    }

    txn.commit().await?;

    Ok(())
}

fn repo_add(pkg_file_name: String) -> anyhow::Result<()> {
    let db_file = format!("{REPO_NAME}.db.tar.gz");

    let output = Command::new("repo-add")
        .args(&[db_file.clone(), pkg_file_name, "--nocolor".to_string()])
        .current_dir("./repo/")
        .output()?;

    if !output.status.success() {
        return Err(anyhow!(
            "Error exit code when repo-add: {}{}",
            String::from_utf8_lossy(output.stdout.as_slice()),
            String::from_utf8_lossy(output.stderr.as_slice())
        ));
    }

    println!("{db_file} updated successfully");
    Ok(())
}

fn repo_remove(pkg_file_name: String) -> anyhow::Result<()> {
    let db_file = format!("{REPO_NAME}.db.tar.gz");

    let output = Command::new("repo-remove")
        .args(&[db_file.clone(), pkg_file_name, "--nocolor".to_string()])
        .current_dir("./repo/")
        .output()?;

    if !output.status.success() {
        return Err(anyhow!(
            "Error exit code when repo-remove: {}{}",
            String::from_utf8_lossy(output.stdout.as_slice()),
            String::from_utf8_lossy(output.stderr.as_slice())
        ));
    }

    println!("{db_file} updated successfully");
    Ok(())
}

pub(crate) async fn rem_ver(
    db: &DatabaseTransaction,
    version: versions::Model,
) -> anyhow::Result<()> {
    if let Some(filename) = version.file_name.clone() {
        // so repo-remove only supports passing a package name and removing the whole package
        // it seems that repo-add removes an older version when called
        // todo fix in future by implementing in rust
        if let Some(pkg) = Packages::find_by_id(version.package_id).one(db).await? {
            // remove from repo db
            repo_remove(pkg.name)?;

            // remove from fs
            fs::remove_file(format!("./repo/{filename}"))?;
        }
    }

    version.delete(db).await?;
    Ok(())
}
