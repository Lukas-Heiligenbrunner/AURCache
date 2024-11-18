use crate::builder::env::limits_from_env;
use crate::builder::logger::BuildLogger;
use crate::builder::makepkg_utils::create_makepkg_config;
use crate::builder::path_utils::create_build_paths;
use anyhow::anyhow;
use bollard::container::{AttachContainerOptions, Config, CreateContainerOptions, LogOutput};
use bollard::image::CreateImageOptions;
use bollard::models::{ContainerCreateResponse, CreateImageInfo, HostConfig};
use bollard::Docker;
use log::{debug, trace};
use rocket::futures::StreamExt;
use std::path::PathBuf;

pub async fn establish_docker_connection() -> anyhow::Result<Docker> {
    let docker = Docker::connect_with_unix_defaults()?;
    docker
        .ping()
        .await
        .map_err(|e| anyhow!("Connection to Docker Socket failed: {}", e))?;
    Ok(docker)
}

/// repull docker image with specified arch
/// returns image id hash
pub async fn repull_image(
    docker: &Docker,
    build_logger: &BuildLogger,
    image: &str,
    arch: String,
) -> anyhow::Result<String> {
    build_logger
        .append(format!("Pulling image: {}", image))
        .await?;
    // repull image to make sure it's up to date
    let mut stream = docker.create_image(
        Some(CreateImageOptions {
            from_image: image,
            platform: arch.as_str(),
            ..Default::default()
        }),
        None,
        None,
    );

    let mut image_id = None;

    while let Some(pull_result) = stream.next().await {
        match pull_result {
            Err(e) => build_logger.append(format!("{}", e)).await?,
            Ok(info @ CreateImageInfo { status: None, .. }) => debug!("{:?}", info),
            Ok(CreateImageInfo { id: Some(id), .. }) => image_id = Some(id),
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

    image_id.ok_or(anyhow!("No Image Id found"))
}

pub async fn create_build_container(
    docker: &Docker,
    build_id: i32,
    name: String,
    arch: String,
    image_id: String,
    build_flags: Vec<&str>,
) -> anyhow::Result<(ContainerCreateResponse, PathBuf)> {
    let (host_build_path_docker, host_active_build_path) = create_build_paths(name.clone())?;

    let build_flags = build_flags.join(" ");
    // create new docker container for current build
    let build_dir_base = "/var/cache/makepkg/pkg";
    let mountpoints = vec![format!("{}:{}", host_build_path_docker, build_dir_base)];

    let (makepkg_config, makepkg_config_path) =
        create_makepkg_config(name.clone(), build_dir_base)?;
    let cmd = format!(
        "cat <<EOF > {}\n{}\nEOF\nparu {} {}",
        makepkg_config_path, makepkg_config, build_flags, name
    );

    let (cpu_limit, memory_limit) = limits_from_env();

    let container_name = format!("aurcache_build_{}_{}", name, build_id);
    let conf = Config {
        image: Some(image_id.as_str()),
        attach_stdout: Some(true),
        attach_stderr: Some(true),
        open_stdin: Some(false),
        user: Some("ab"),
        cmd: Some(vec!["sh", "-c", cmd.as_str()]),
        host_config: Some(HostConfig {
            auto_remove: Some(true),
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
                platform: Some(arch.as_str()),
            }),
            conf,
        )
        .await?;
    Ok((create_info, host_active_build_path))
}

pub async fn monitor_build_output(
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
