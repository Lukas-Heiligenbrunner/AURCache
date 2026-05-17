use crate::build::Builder;
use crate::build_mode::{BuildMode, get_build_mode};
use crate::logger::BuildLogger;
use crate::makepkg_utils::{create_makepkg_config, create_pacman_config};
use anyhow::anyhow;
use aurcache_db::helpers::active_value_ext::ActiveValueExt;
use aurcache_db::packages::SourceData;
use aurcache_types::settings::{ApplicationSettings, Setting, SettingsEntry};
use aurcache_utils::git::checkout::checkout_repo_ref;
use aurcache_utils::settings::general::SettingsTraits;
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
                        platforms: None,
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
        let host_build_dir = match get_build_mode() {
            BuildMode::DinD(cfg) => cfg.build_path,
            BuildMode::Host(cfg) => cfg.build_artifact_dir_host,
        };
        let container_pkgdest_dir = Path::new("/build");
        let container_build_dir = container_pkgdest_dir.join("src");
        let mountpoints = vec![format!(
            "{host_build_dir}/{name}:{builder_root}",
            builder_root = container_pkgdest_dir.display()
        )];

        let mut mounts = vec![];

        // Mount a custom mirrorlist if one exists on the host.
        // If absent, the builder image's default mirrorlist is used.
        if arch == "linux/x86_64" {
            let archlinux_mirrorlist_path = "/etc/pacman.d/mirrorlist";
            let mirrorlist_source = match get_build_mode() {
                BuildMode::DinD(cfg) => format!("{}/mirrorlist", cfg.mirrorlist_path),
                BuildMode::Host(cfg) => format!("{}/mirrorlist", cfg.mirrorlist_path_host),
            };

            if !mirrorlist_source.starts_with('/')
                || std::path::Path::new(&mirrorlist_source).exists()
            {
                let mnt = match get_build_mode() {
                    BuildMode::DinD(_) => Mount {
                        target: Some(archlinux_mirrorlist_path.to_string()),
                        source: Some(mirrorlist_source),
                        typ: Some(MountTypeEnum::BIND),
                        read_only: Some(false),
                        ..Default::default()
                    },
                    BuildMode::Host(_cfg) => {
                        if mirrorlist_source.starts_with('/') {
                            Mount {
                                target: Some(archlinux_mirrorlist_path.to_string()),
                                source: Some(mirrorlist_source),
                                typ: Some(MountTypeEnum::BIND),
                                read_only: Some(false),
                                ..Default::default()
                            }
                        } else {
                            let (volume_name, subpath) = mirrorlist_source
                                .split_once('/')
                                .ok_or(anyhow!("Mirrorlist path not containing '/': Invalid"))?;

                            Mount {
                                target: Some(archlinux_mirrorlist_path.to_string()),
                                source: Some(volume_name.to_string()),
                                typ: Some(MountTypeEnum::VOLUME),
                                read_only: Some(false),
                                volume_options: Some(MountVolumeOptions {
                                    subpath: Some(format!("{subpath}/mirrorlist")),
                                    ..Default::default()
                                }),
                                ..Default::default()
                            }
                        }
                    }
                };
                mounts.push(mnt);
            }
        }

        // Mount the AURCache repo into the builder container so makepkg -s
        // can resolve dependencies that were previously built by AURCache.
        let aurcache_repo_mount = "/aurcache-repo";
        let repo_host_path = match get_build_mode() {
            BuildMode::DinD(cfg) => cfg.repo_path,
            BuildMode::Host(cfg) => cfg.repo_host_path,
        };
        mounts.push(Mount {
            target: Some(aurcache_repo_mount.to_string()),
            source: Some(repo_host_path.clone()),
            typ: Some(MountTypeEnum::BIND),
            read_only: Some(true),
            ..Default::default()
        });

        let pkg_id = *self.package_model.id.get()?;
        let (makepkg_config, makepkg_config_path) =
            create_makepkg_config(&self.db, pkg_id, container_pkgdest_dir).await?;

        let pacman_config = create_pacman_config(&self.db, pkg_id, aurcache_repo_mount).await;

        let source_data = SourceData::from_str(self.package_model.source_data.get()?)?;
        let pkgbase = self.package_model.pkgbase.get()?;

        let build_cmd = crate::commands::build_build_command(
            &source_data,
            pkgbase,
            &build_flags,
            &container_build_dir,
        );

        let cmd = crate::commands::wrap_with_makepkg_config(
            &makepkg_config,
            &makepkg_config_path,
            &pacman_config,
            &build_cmd,
        );
        info!("Build command: {build_cmd}");

        let cpu_limit: SettingsEntry<u64> = ApplicationSettings::get(
            Setting::CpuLimit,
            Some(*self.package_model.id.get()?),
            &self.db,
        )
        .await;
        // we store cpu in uCPU in db
        let cpu_limit = cpu_limit.value * 1_000_000;
        let memory_limit: SettingsEntry<i64> = ApplicationSettings::get(
            Setting::MemoryLimit,
            Some(*self.package_model.id.get()?),
            &self.db,
        )
        .await;
        // we store memory limit in mb in db
        let memory_limit = memory_limit.value * 1024 * 1024;

        // docker container names must match [a-zA-Z0-9][a-zA-Z0-9_.-]* regex
        let filtered_name: String = name.chars().filter(|c| c.is_alphanumeric()).collect();

        let build_id = self.build_model.id.get()?;
        let container_name = format!("aurcache_build_{filtered_name}_{build_id}");
        let auto_remove = cfg!(not(debug_assertions));
        let conf = ContainerCreateBody {
            image: Some(image_name.to_string()),
            attach_stdout: Some(true),
            attach_stderr: Some(true),
            open_stdin: Some(false),
            user: Some("ab".to_string()),
            cmd: Some(vec!["sh".to_string(), "-lec".to_string(), cmd]),
            host_config: Some(HostConfig {
                auto_remove: Some(auto_remove),
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
