use crate::builder::build::Builder;
use crate::builder::env::limits_from_env;
use crate::builder::logger::BuildLogger;
use crate::builder::makepkg_utils::create_makepkg_config;
use crate::builder::path_utils::create_build_paths;
use crate::utils::db::ActiveValueExt;
use anyhow::anyhow;
use bollard::Docker;
use bollard::container::LogOutput;
use bollard::models::{ContainerCreateBody, ContainerCreateResponse, CreateImageInfo, HostConfig};
use bollard::query_parameters::{AttachContainerOptions, CreateContainerOptions};
use bollard::query_parameters::{CreateImageOptions, ListImagesOptions, RemoveImageOptions};
use itertools::Itertools;
use log::{debug, info, trace};
use rocket::futures::StreamExt;
use std::collections::HashMap;
use std::path::PathBuf;

impl Builder {
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
    pub async fn repull_image(&self, image: &str, arch: String) -> anyhow::Result<()> {
        self.logger
            .append(format!("Pulling image: {}", image))
            .await;
        // repull image to make sure it's up to date
        let mut stream = self.docker.create_image(
            Some(CreateImageOptions {
                from_image: Some(image.to_string()),
                platform: arch,
                ..Default::default()
            }),
            None,
            None,
        );

        let mut image_id = None;

        while let Some(pull_result) = stream.next().await {
            match pull_result {
                Err(e) => self.logger.append(format!("{}", e)).await,
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
                        self.logger.append(status.clone()).await;
                    }
                },
            }
        }

        let image_id = image_id.ok_or(anyhow!("No Image Id found after pulling: {}", image))?;
        debug!(
            "Build #{}: Image pulled with id: {}",
            self.build_model.id.get()?,
            image_id
        );

        // Delete untagged (dangling) images after pulling a new one.
        self.cleanup_untagged_images().await?;
        Ok(())
    }

    /// Remove all untagged (dangling) images from Docker.
    pub async fn cleanup_untagged_images(&self) -> anyhow::Result<()> {
        // Create a filter to list only dangling images.
        let mut filters = HashMap::new();
        filters.insert("dangling".to_string(), vec!["true".to_string()]);

        let list_options = Some(ListImagesOptions {
            all: false,
            filters: Some(filters),
            ..Default::default()
        });

        let images = self.docker.list_images(list_options).await?;
        for image in images {
            self.logger
                .append(format!("Removing untagged image: {}", image.id))
                .await;
            // force remove images
            self.docker
                .remove_image(
                    &image.id,
                    Some(RemoveImageOptions {
                        force: true,
                        noprune: false,
                    }),
                    None,
                )
                .await?;
        }
        Ok(())
    }

    pub async fn create_build_container(
        &self,
        arch: String,
        image_name: &str,
    ) -> anyhow::Result<(ContainerCreateResponse, PathBuf)> {
        let name = self.package_model.name.get()?;
        let (host_build_path_docker, host_active_build_path) = create_build_paths(name.clone())?;

        let build_flags = self.package_model.build_flags.get()?.split(";").join(" ");
        // create new docker container for current build
        let build_dir_base = "/var/cache/makepkg/pkg";
        let mut mountpoints = vec![format!("{}:{}", host_build_path_docker, build_dir_base)];

        // todo allow for custom mirrorlists for other archs
        if arch == "linux/x86_64" {
            let mirrorlist_path = "./config/mirrorlist_x86_64";
            mountpoints.push(format!("{}:/etc/pacman.d/mirrorlist", mirrorlist_path))
        }

        let (makepkg_config, makepkg_config_path) =
            create_makepkg_config(name.clone(), build_dir_base)?;
        let build_cmd = format!("paru {} {}", build_flags, name);
        info!("Build command: {}", build_cmd);
        let cmd = format!(
            "cat <<EOF > {}\n{}\nEOF\n{}",
            makepkg_config_path, makepkg_config, build_cmd
        );

        let (cpu_limit, memory_limit) = limits_from_env();

        // docker container names must match [a-zA-Z0-9][a-zA-Z0-9_.-]* regex
        let filtered_name: String = name.chars().filter(|c| c.is_alphanumeric()).collect();

        let build_id = self.build_model.id.get()?;
        let container_name = format!("aurcache_build_{}_{}", filtered_name, build_id);
        let conf = ContainerCreateBody {
            image: Some(image_name.to_string()),
            attach_stdout: Some(true),
            attach_stderr: Some(true),
            open_stdin: Some(false),
            user: Some("ab".to_string()),
            cmd: Some(vec!["sh".to_string(), "-c".to_string(), cmd]),
            host_config: Some(HostConfig {
                auto_remove: Some(true),
                nano_cpus: Some(cpu_limit as i64),
                memory_swap: Some(memory_limit),
                binds: Some(mountpoints),
                ..Default::default()
            }),
            ..Default::default()
        };
        let create_info = self
            .docker
            .create_container(
                Some(CreateContainerOptions {
                    name: Some(container_name),
                    platform: arch,
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
                Some(AttachContainerOptions {
                    stdout: true,
                    stderr: true,
                    stdin: false,
                    stream: true,
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
                            .await
                    }
                    LogOutput::StdErr { message } => {
                        build_logger
                            .append(String::from_utf8_lossy(&message).into_owned())
                            .await
                    }
                },
                Err(e) => build_logger.append(e.to_string()).await,
            }
        }
        Ok(())
    }
}
