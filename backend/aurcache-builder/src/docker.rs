use crate::build::Builder;
use crate::build_mode::{BuildMode, get_build_mode};
use crate::env::limits_from_env;
use crate::git::checkout::checkout_repo_ref;
use crate::logger::BuildLogger;
use crate::makepkg_utils::create_makepkg_config;
use anyhow::anyhow;
use aurcache_db::helpers::active_value_ext::ActiveValueExt;
use aurcache_db::packages::SourceData;
use bollard::container::LogOutput;
use bollard::models::{
    ContainerCreateBody, ContainerCreateResponse, CreateImageInfo, HostConfig, Mount,
    MountTypeEnum, MountVolumeOptions,
};
use bollard::query_parameters::{
    AttachContainerOptions, CreateContainerOptions, UploadToContainerOptions,
};
use bollard::query_parameters::{CreateImageOptions, ListImagesOptions, RemoveImageOptions};
use bollard::{Docker, body_try_stream};
use flate2::Compression;
use flate2::write::GzEncoder;
use futures::{StreamExt, TryFutureExt};
use itertools::Itertools;
use std::collections::HashMap;
use std::path::Path;
use std::str::FromStr;
use tempfile::tempdir;
use tokio::fs::File;
use tokio_util::io::ReaderStream;
use tracing::{debug, info, trace};

/// git repo path inside builder container in git build mode
static GIT_REPO_PATH: &str = "/tmp";

impl Builder {
    pub async fn establish_docker_connection() -> anyhow::Result<Docker> {
        let docker = Docker::connect_with_unix_defaults()?;
        docker
            .ping()
            .await
            .map_err(|e| anyhow!("Connection to Docker Socket failed: {e}
If using podman remember to install 'podman-docker' to mimic the docker socket
or if you run podman rootless to start the user service with 'systemctl --user start podman.socket'
and check also if the 'DOCKER_HOST=unix:///var/run/user/1000/podman/podman.sock' env variable is set to the correct docker socket!"))?;
        Ok(docker)
    }

    /// repull docker image with specified arch
    /// returns image id hash
    pub async fn repull_image(&self, image: &str, arch: String) -> anyhow::Result<()> {
        self.logger.append(format!("Pulling image: {image}")).await;
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
                Err(e) => self.logger.append(format!("{e}")).await,
                Ok(info @ CreateImageInfo { status: None, .. }) => debug!("{info:?}"),
                Ok(CreateImageInfo { id: Some(id), .. }) => image_id = Some(id),
                Ok(
                    ref info @ CreateImageInfo {
                        status: Some(ref status),
                        ..
                    },
                ) => match status.as_str() {
                    "Pulling fs layer" | "Waiting" | "Downloading" | "Verifying Checksum"
                    | "Extracting" => {
                        trace!("{info:?}");
                    }
                    _ => {
                        self.logger.append(status.clone()).await;
                    }
                },
            }
        }

        let image_id = image_id.ok_or(anyhow!("No Image Id found after pulling: {image}"))?;
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
    ) -> anyhow::Result<ContainerCreateResponse> {
        let name = self.package_model.name.get()?;

        let build_flags = self.package_model.build_flags.get()?.split(';').join(" ");
        // create new docker container for current build
        let build_dir_base = "/var/cache/makepkg/pkg";
        let host_build_path_docker = match get_build_mode() {
            BuildMode::DinD(cfg) => cfg.aurcache_build_path,
            BuildMode::Host(cfg) => cfg.build_artifact_dir_host,
        };
        let mountpoints = vec![format!("{}:{}", host_build_path_docker, build_dir_base)];

        let mut mounts = vec![];

        // todo allow for custom mirrorlists for other archs
        if arch == "linux/x86_64" {
            let archlinux_mirrorlist_path = "/etc/pacman.d";
            let mnt = match get_build_mode() {
                BuildMode::DinD(cfg) => {
                    let mirrorlist_path = cfg.mirrorlist_path;

                    Mount {
                        target: Some(archlinux_mirrorlist_path.to_string()),
                        source: Some(mirrorlist_path.clone()),
                        typ: Some(MountTypeEnum::BIND),
                        read_only: Some(false),
                        ..Default::default()
                    }
                }
                BuildMode::Host(cfg) => {
                    let mirrorlist_path = cfg.mirrorlist_path_host;
                    if mirrorlist_path.starts_with('/') {
                        Mount {
                            target: Some(archlinux_mirrorlist_path.to_string()),
                            source: Some(mirrorlist_path.clone()),
                            typ: Some(MountTypeEnum::BIND),
                            read_only: Some(false),
                            ..Default::default()
                        }
                    } else {
                        let (volume_name, subpath) = mirrorlist_path
                            .split_once('/')
                            .ok_or(anyhow!("Mirrorlist path not containing '/': Invalid"))?;

                        Mount {
                            target: Some(archlinux_mirrorlist_path.to_string()),
                            source: Some(volume_name.to_string()),
                            typ: Some(MountTypeEnum::VOLUME),
                            read_only: Some(false),
                            volume_options: Some(MountVolumeOptions {
                                subpath: Some(subpath.to_string()),
                                ..Default::default()
                            }),
                            ..Default::default()
                        }
                    }
                }
            };
            mounts.push(mnt);
        }

        let (makepkg_config, makepkg_config_path) =
            create_makepkg_config(name.clone(), build_dir_base)?;

        let source_data = SourceData::from_str(self.package_model.source_data.get()?)?;
        let build_cmd = match source_data {
            SourceData::Aur { .. } => {
                let build_cmd = format!("paru {build_flags} {name}");
                // first update the package list, then update trustdb and then build cmd
                let steps = [
                    "sudo pacman -Sy --noconfirm",
                    "sudo pacman-key --init",
                    "sudo pacman-key --populate archlinux",
                    build_cmd.as_str(),
                ];
                steps.join(" && ")
            }
            SourceData::Git { .. } => {
                // somehow we need also to `cd` into the repo dir, otherwise builds fail
                let build_cmd = format!(
                    "sudo chmod -R 1777 {GIT_REPO_PATH} && cd {GIT_REPO_PATH} && paru {build_flags} {GIT_REPO_PATH}"
                );
                // first update the package list, then update trustdb and then build cmd
                let steps = [
                    "sudo pacman -Sy --noconfirm",
                    "sudo pacman-key --init",
                    "sudo pacman-key --populate archlinux",
                    build_cmd.as_str(),
                ];
                steps.join(" && ")
            }
            SourceData::Upload { .. } => {
                todo!("unpack zip and store it in build container dir")
            }
        };

        info!("Build command: {build_cmd}");
        let cmd = format!("cat <<EOF > {makepkg_config_path}\n{makepkg_config}\nEOF\n{build_cmd}");

        let (cpu_limit, memory_limit) = limits_from_env();

        // docker container names must match [a-zA-Z0-9][a-zA-Z0-9_.-]* regex
        let filtered_name: String = name.chars().filter(|c| c.is_alphanumeric()).collect();

        let build_id = self.build_model.id.get()?;
        let container_name = format!("aurcache_build_{filtered_name}_{build_id}");
        let conf = ContainerCreateBody {
            image: Some(image_name.to_string()),
            attach_stdout: Some(true),
            attach_stderr: Some(true),
            open_stdin: Some(false),
            user: Some("ab".to_string()),
            cmd: Some(vec![
                "sh".to_string(),
                "-l".to_string(),
                "-c".to_string(),
                cmd,
            ]),
            host_config: Some(HostConfig {
                #[cfg(debug_assertions)]
                auto_remove: Some(false),
                #[cfg(not(debug_assertions))]
                auto_remove: Some(false),
                nano_cpus: Some(cpu_limit as i64),
                memory_swap: Some(memory_limit),
                binds: Some(mountpoints),
                mounts: Some(mounts),
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

        match source_data {
            SourceData::Git {
                url,
                r#ref,
                subfolder,
            } => {
                self.git_checkout_to_container(
                    create_info.id.clone(),
                    GIT_REPO_PATH.to_string(),
                    url,
                    r#ref,
                    subfolder,
                )
                .await?;
            }
            SourceData::Upload { .. } => {
                todo!("Unpack zip into build container")
            }
            _ => {}
        }

        Ok(create_info)
    }

    /// Create a .tar.gz archive from a directory
    async fn create_tar_gz(
        src_dir: &Path,
        dest_path: &Path,
        subfolder: String,
    ) -> anyhow::Result<()> {
        let tar_gz = std::fs::File::create(dest_path)?;
        let enc = GzEncoder::new(tar_gz, Compression::default());
        let mut tar = tar::Builder::new(enc);
        tar.append_dir_all(".", src_dir.join(subfolder))?;
        tar.finish()?;
        Ok(())
    }

    /// checkout a git repo into a docker container
    async fn git_checkout_to_container(
        &self,
        container_id: String,
        path: String,
        git_repo: String,
        git_ref: String,
        git_subfolder: String,
    ) -> anyhow::Result<()> {
        info!("Cloning repository {git_repo}...");

        let dir = tempdir()?;
        let repo_dir = dir.path().join("repo");

        checkout_repo_ref(git_repo, git_ref.clone(), repo_dir.clone())?;
        info!("Checked out {:?}", git_ref);

        // Create a tar.gz of the cloned repo
        let tar_path = dir.path().join("repo.tar.gz");
        debug!("Creating tar archive at {:?}", tar_path);
        Self::create_tar_gz(&repo_dir, &tar_path, git_subfolder).await?;

        let options = Some(UploadToContainerOptions {
            path,
            copy_uidgid: Some("false".to_string()),
            ..Default::default()
        });

        let file = File::open(tar_path)
            .map_ok(ReaderStream::new)
            .try_flatten_stream();

        self.docker
            .upload_to_container(container_id.as_str(), options, body_try_stream(file))
            .await
            .expect("upload failed");

        _ = dir.close();
        Ok(())
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
                            .await;
                    }
                    LogOutput::StdErr { message } => {
                        build_logger
                            .append(String::from_utf8_lossy(&message).into_owned())
                            .await;
                    }
                },
                Err(e) => build_logger.append(e.to_string()).await,
            }
        }
        Ok(())
    }
}
