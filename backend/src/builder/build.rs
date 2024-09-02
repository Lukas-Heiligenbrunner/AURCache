use crate::builder::logger::BuildLogger;
use crate::db::files::ActiveModel;
use crate::db::migration::JoinType;
use crate::db::prelude::{Builds, Files, PackagesFiles};
use crate::db::{builds, files, packages, packages_files};
use crate::repo::utils::try_remove_archive_file;
use anyhow::anyhow;
use bollard::container::{
    AttachContainerOptions, Config, CreateContainerOptions, KillContainerOptions, LogOutput,
    RemoveContainerOptions,
};
use bollard::image::CreateImageOptions;
use bollard::models::{
    ContainerCreateResponse, ContainerStateStatusEnum, CreateImageInfo, HostConfig,
};
use bollard::{ClientVersion, Docker, API_DEFAULT_VERSION};
use log::{debug, info, trace};
use rocket::futures::StreamExt;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, ModelTrait, QueryFilter,
    QuerySelect, RelationTrait, Set, TransactionTrait,
};
use std::collections::HashMap;
use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{env, fs};
use tokio::sync::Mutex;
use tokio::time::timeout;

static BUILDER_IMAGE: &str = "ghcr.io/lukas-heiligenbrunner/aurcache-builder:latest";
static QEMU_IMAGE: &str = "multiarch/qemu-user-static:latest";

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
                SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64,
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
                SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64,
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
    let docker = Docker::connect_with_unix_defaults()?;

    let target_arch = "linux/arm64/v8";
    let host_arch = "linux/x86_64";

    docker
        .ping()
        .await
        .map_err(|e| anyhow!("Connection to Docker Socket failed: {}", e))?;

    if host_arch != target_arch {
        if host_arch != "linux/x86_64" {
            return Err(anyhow!(
                "Unsupported host architecture {} for cross-compile",
                host_arch
            ));
        }
        //repull_image(&docker, &build_logger, QEMU_IMAGE, "").await?;
    }
    repull_image(&docker, &build_logger, BUILDER_IMAGE, target_arch).await?;

    // prepare cross-build
    if host_arch != target_arch {
        println!("creating qemu container");
        //let create_info = create_qemu_container(&docker).await?;
        //println!("starting qemu container");
        //docker
        //    .start_container::<String>(&create_info.id, None)
        //    .await?;
        // wait until the container exited
        //println!("waiting for qemu container to exit");
        //while docker.inspect_container(&create_info.id, None).await.ok().is_some() {
        //    println!("waiting for qemu container to exit");
        //    tokio::time::sleep(Duration::from_secs(1)).await;
        //}
    }

    let (create_info, host_active_build_path) =
        create_build_container(&docker, build_id, name.clone()).await?;
    let id = create_info.id;

    // start build container
    docker.start_container::<String>(&id, None).await?;

    // insert container id to container map
    job_containers.lock().await.insert(build_id, id.clone());

    // monitor build output
    match timeout(
        job_timeout_from_env(),
        monitor_build_output(&build_logger, &docker, id.clone()),
    )
    .await
    {
        Ok(v) => v?,
        Err(_) => {
            build_logger
                .append(format!("Build #{build_id} timed out for package '{name}'"))
                .await?;
            // kill build container
            docker
                .kill_container(&id, Some(KillContainerOptions { signal: "SIGKILL" }))
                .await?;
        }
    }

    // remove build from container map
    job_containers
        .lock()
        .await
        .remove(&build_id)
        .ok_or(anyhow!(
            "Failed to remove build container from active builds map"
        ))?;

    // move built tar.gz archives to host and repo-add
    move_and_add_pkgs(&build_logger, host_active_build_path.clone(), pkg_id, db).await?;
    // remove active build dir
    fs::remove_dir(host_active_build_path)?;
    Ok(())
}

async fn monitor_build_output(
    build_logger: &BuildLogger,
    docker: &Docker,
    id: String,
) -> anyhow::Result<()> {
    let mut attach_results = docker
        .attach_container(
            &id,
            Some(AttachContainerOptions::<String> {
                stdout: Some(true),
                stderr: Some(true),
                stdin: Some(false),
                stream: Some(true),
                ..Default::default()
            }),
        )
        .await?;

    while let Some(log_result) = attach_results.output.next().await {
        match log_result {
            Ok(chunk) => match chunk {
                LogOutput::StdIn { .. } => unreachable!(),
                LogOutput::Console { .. } => unreachable!(),
                LogOutput::StdOut { message } => {
                    build_logger
                        .append(String::from_utf8_lossy(&message).into_owned())
                        .await?
                }
                LogOutput::StdErr { message } => {
                    build_logger
                        .append(String::from_utf8_lossy(&message).into_owned())
                        .await?
                }
            },
            Err(e) => build_logger.append(e.to_string()).await?,
        }
    }
    Ok(())
}

async fn create_qemu_container(docker: &Docker) -> anyhow::Result<ContainerCreateResponse> {
    let conf = Config {
        image: Some(QEMU_IMAGE),
        attach_stdout: Some(true),
        attach_stderr: Some(true),
        open_stdin: Some(false),
        cmd: Some(vec!["--reset", "-p", "yes"]),
        host_config: Some(HostConfig {
            auto_remove: Some(true),
            privileged: Some(true),
            ..Default::default()
        }),
        ..Default::default()
    };

    let create_info = docker
        .create_container::<&str, &str>(
            Some(CreateContainerOptions {
                name: "qemu",
                platform: None,
            }),
            conf,
        )
        .await?;
    Ok(create_info)
}

async fn create_build_container(
    docker: &Docker,
    build_id: i32,
    name: String,
) -> anyhow::Result<(ContainerCreateResponse, PathBuf)> {
    let (host_build_path_docker, host_active_build_path) = create_build_paths(name.clone())?;

    // create new docker container for current build
    let build_dir_base = "/var/cache/makepkg/pkg";
    let mountpoints = vec![format!("{}:{}", host_build_path_docker, build_dir_base)];

    let (makepkg_config, makepkg_config_path) =
        create_makepkg_config(name.clone(), build_dir_base)?;
    let cmd = format!(
        "cat <<EOF > {}\n{}\nEOF\nparu -Syu --noconfirm --noprogressbar --color never {}",
        makepkg_config_path, makepkg_config, name
    );

    let (cpu_limit, memory_limit) = limits_from_env();

    let container_name = format!("aurcache_build_{}_{}", name, build_id);
    let conf = Config {
        image: Some(BUILDER_IMAGE),
        attach_stdout: Some(true),
        attach_stderr: Some(true),
        open_stdin: Some(false),
        user: Some("ab"),
        cmd: Some(vec!["sh", "-c", cmd.as_str()]),
        host_config: Some(HostConfig {
            //auto_remove: Some(true),
            nano_cpus: Some(cpu_limit as i64),
            memory_swap: Some(memory_limit),
            binds: Some(mountpoints),
            ..Default::default()
        }),
        ..Default::default()
    };
    let create_info = docker
        .create_container::<&str, &str>(
            Some(CreateContainerOptions {
                name: container_name.as_str(),
                platform: Some("linux/arm64/v8"),
            }),
            conf,
        )
        .await?;
    Ok((create_info, host_active_build_path))
}

fn create_makepkg_config(name: String, build_dir_base: &str) -> anyhow::Result<(String, String)> {
    let makepkg_config = format!(
        "\
MAKEFLAGS=-j$(nproc)
PKGDEST={}/{}",
        build_dir_base, name
    );
    let makepkg_config_path = "/var/ab/.config/pacman/makepkg.conf";
    Ok((makepkg_config, makepkg_config_path.to_string()))
}

fn create_build_paths(name: String) -> anyhow::Result<(String, PathBuf)> {
    // create builds dir
    let mut host_build_path_base = env::current_dir()?;
    host_build_path_base.push("builds");
    fs::create_dir_all(host_build_path_base.clone())?;
    fs::set_permissions(host_build_path_base.clone(), Permissions::from_mode(0o777))?;

    // create current build dir
    let mut host_active_build_path = host_build_path_base.clone();
    host_active_build_path.push(name);
    fs::create_dir_all(host_active_build_path.clone())?;
    fs::set_permissions(
        host_active_build_path.clone(),
        Permissions::from_mode(0o777),
    )?;

    // use either docker volume or base dir as docker host mount path
    let host_build_path_docker =
        env::var("BUILD_ARTIFACT_DIR").unwrap_or(host_build_path_base.display().to_string());
    Ok((host_build_path_docker, host_active_build_path))
}

fn job_timeout_from_env() -> Duration {
    let job_timeout = env::var("JOB_TIMEOUT")
        .ok()
        .and_then(|x| x.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(60 * 60));
    debug!("job_timeout: {} sec", job_timeout.as_secs());
    job_timeout
}

fn limits_from_env() -> (u64, i64) {
    // cpu_limit in milli cpus
    let cpu_limit = env::var("CPU_LIMIT")
        .ok()
        .and_then(|x| x.parse::<u64>().ok())
        .map(|x| x * 1_000_000)
        .unwrap_or(0);
    debug!("cpu_limit: {} mCPUs", cpu_limit);
    // memory_limit in megabytes
    let memory_limit = env::var("MEMORY_LIMIT")
        .ok()
        .and_then(|x| x.parse::<i64>().ok())
        .map(|x| x * 1024 * 1024)
        .unwrap_or(-1);
    debug!("memory_limit: {}MB", memory_limit);
    (cpu_limit, memory_limit)
}

async fn repull_image(
    docker: &Docker,
    build_logger: &BuildLogger,
    image: &str,
    arch: &str,
) -> anyhow::Result<()> {
    build_logger
        .append(format!("Pulling image: {}", image))
        .await?;
    // repull image to make sure it's up to date
    let mut stream = docker.create_image(
        Some(CreateImageOptions {
            from_image: image,
            platform: arch,
            ..Default::default()
        }),
        None,
        None,
    );

    while let Some(pull_result) = stream.next().await {
        match pull_result {
            Err(e) => build_logger.append(format!("{}", e)).await?,
            Ok(info @ CreateImageInfo { status: None, .. }) => debug!("{:?}", info),
            Ok(
                ref info @ CreateImageInfo {
                    status: Some(ref status),
                    ..
                },
            ) => match status.as_str() {
                "Pulling fs layer" | "Waiting" | "Downloading" | "Verifying Checksum"
                | "Extracting" => {
                    trace!("{:?}", info);
                }
                _ => {
                    build_logger.append(status.clone()).await?;
                }
            },
        }
    }
    Ok(())
}

/// move built files from build container to host and add them to the repo
async fn move_and_add_pkgs(
    build_logger: &BuildLogger,
    host_build_path: PathBuf,
    pkg_id: i32,
    db: &DatabaseConnection,
) -> anyhow::Result<()> {
    let archive_paths = fs::read_dir(host_build_path.clone())?.collect::<Vec<_>>();
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

        pacman_repo_utils::repo_add(
            format!("./repo/{}", archive_name).as_str(),
            "./repo/repo.db.tar.gz".to_string(),
            "./repo/repo.files.tar.gz".to_string(),
        )?;
        info!("Successfully added '{}' to the repo archive", archive_name);
    }
    txn.commit().await?;
    Ok(())
}
