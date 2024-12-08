use crate::builder::build::Builder;
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
use itertools::Itertools;
use crate::utils::db::ActiveValueExt;

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
    pub async fn repull_image(&self, image: &str, arch: String) -> anyhow::Result<String> {
        self.logger
            .append(format!("Pulling image: {}", image))
            .await;
        // repull image to make sure it's up to date
        let mut stream = self.docker.create_image(
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

        image_id.ok_or(anyhow!("No Image Id found"))
    }

    pub async fn create_build_container(
        &self,
        arch: String,
        image_id: String,
    ) -> anyhow::Result<(ContainerCreateResponse, PathBuf)> {
        let name = self.package_model.name.get()?;
        let (host_build_path_docker, host_active_build_path) = create_build_paths(name.clone())?;

        let build_flags = self.package_model.build_flags.get()?.split(";").join(" ");
        // create new docker container for current build
        let build_dir_base = "/var/cache/makepkg/pkg";
        let mountpoints = vec![format!("{}:{}", host_build_path_docker, build_dir_base)];

        let (makepkg_config, makepkg_config_path) =
            create_makepkg_config(name.clone(), build_dir_base)?;
        let build_cmd = format!("paru {} {}", build_flags, name);
        debug!("Build command: {}", build_cmd);
        let cmd = format!(
            "cat <<EOF > {}\n{}\nEOF\n{}",
            makepkg_config_path, makepkg_config, build_cmd
        );

        let (cpu_limit, memory_limit) = limits_from_env();

        let build_id = self.build_model.id.get()?;
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
        let create_info = self
            .docker
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
