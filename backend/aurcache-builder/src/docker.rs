use crate::build::Builder;
use crate::build_mode::{BuildMode, get_build_mode, get_repo_config};
use crate::logger::BuildLogger;
use crate::makepkg_utils::{create_makepkg_config, create_pacman_config};
use anyhow::anyhow;
use aurcache_db::helpers::active_value_ext::ActiveValueExt;
use aurcache_types::settings::{ApplicationSettings, Setting, SettingsEntry};
use aurcache_utils::settings::general::SettingsTraits;
use bollard::container::LogOutput;
use bollard::models::{
    ContainerCreateBody, ContainerCreateResponse, CreateImageInfo, EndpointSettings, HostConfig,
    Mount, MountTypeEnum, MountVolumeOptions, NetworkingConfig,
};
use bollard::query_parameters::{
    AttachContainerOptions, CreateContainerOptions, UploadToContainerOptions,
};
use bollard::query_parameters::{CreateImageOptions, ListImagesOptions, RemoveImageOptions};
use bollard::{Docker, body_full};
use futures::StreamExt;
use itertools::Itertools;
use std::collections::HashMap;
use std::path::Path;
use tracing::{debug, info, trace};

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
    pub async fn repull_image(&self, image: &str, platform: String) -> anyhow::Result<()> {
        self.logger.append(format!("Pulling image: {image}")).await;
        // repull image to make sure it's up to date
        let mut stream = self.docker.create_image(
            Some(CreateImageOptions {
                from_image: Some(image.to_string()),
                platform,
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
        platform: String,
        image_name: &str,
    ) -> anyhow::Result<ContainerCreateResponse> {
        let name = self.package_model.name.get()?;

        let build_flags = self.package_model.build_flags.get()?.split(';').join(" ");
        // create new docker container for current build
        let host_build_dir = match get_build_mode() {
            BuildMode::DinD(cfg) => cfg.build_path,
            BuildMode::Host(cfg) => cfg.build_artifact_dir_host,
        };
        let container_pkgdest_dir = Path::new(crate::commands::CONTAINER_PKGDEST_DIR);
        let container_build_dir = Path::new(crate::commands::CONTAINER_BUILD_DIR);
        let mountpoints = vec![format!(
            "{host_build_dir}/{name}:{builder_root}",
            builder_root = container_pkgdest_dir.display()
        )];

        let mut mounts = vec![];

        // Mount the mirrorlist into the builder container for x86_64 builds.
        // Startup guarantees the file exists. Non-x86_64 builds skip this block
        // because their mirrorlist would differ.
        if let Some(mnt) = mirrorlist_mount(&platform)? {
            mounts.push(mnt);
        }

        let pkg_id = *self.package_model.id.get()?;
        let (makepkg_config, makepkg_config_path) =
            create_makepkg_config(Some((&self.db, pkg_id)), container_pkgdest_dir).await?;

        let repo_config = get_repo_config(&self.docker).await?;
        let pacman_config = create_pacman_config(&self.db, pkg_id, &repo_config.url).await;

        let source_data = self.package_model.source_data.get()?;
        let pkgbase = self.package_model.name.get()?;

        let build_cmd =
            crate::commands::build_build_command(pkgbase, &build_flags, container_build_dir);

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
            cmd: Some(vec![
                "bash".to_string(),
                "-leco".to_string(),
                "pipefail".to_string(),
                cmd,
            ]),
            host_config: Some(HostConfig {
                auto_remove: Some(auto_remove),
                nano_cpus: Some(cpu_limit as i64),
                memory_swap: Some(memory_limit),
                binds: Some(mountpoints),
                mounts: Some(mounts),
                network_mode: repo_config.host_network.then(|| "host".to_string()),
                ..Default::default()
            }),
            networking_config: repo_config.builder_network.as_deref().map(|network| {
                NetworkingConfig {
                    endpoints_config: Some(HashMap::from([(
                        network.to_string(),
                        EndpointSettings::default(),
                    )])),
                }
            }),
            ..Default::default()
        };
        let create_info = self
            .docker
            .create_container(
                Some(CreateContainerOptions {
                    name: Some(container_name),
                    platform,
                }),
                conf,
            )
            .await?;

        // Upload the source archive to the build container.
        // Docker's upload_to_container API expects a raw tar stream, so we
        // decompress the gzipped archive before sending it.
        let archive_bytes = self
            .store
            .archive_bytes(&self.client, source_data)
            .await
            .map_err(|e| anyhow!("Failed to get source archive: {e}"))?;

        self.upload_source_archive(
            create_info.id.clone(),
            container_build_dir.to_string_lossy().as_ref(),
            &archive_bytes,
        )
        .await?;

        Ok(create_info)
    }

    /// Upload the source archive to the build container.
    ///
    /// Docker's `upload_to_container` auto-detects gzip compression, so we
    /// send the raw `.tar.gz` bytes as-is.
    async fn upload_source_archive(
        &self,
        container_id: String,
        container_build_dir: &str,
        archive_bytes: &[u8],
    ) -> anyhow::Result<()> {
        let options = Some(UploadToContainerOptions {
            path: container_build_dir.to_string(),
            copy_uidgid: Some("false".to_string()),
            ..Default::default()
        });

        self.docker
            .upload_to_container(
                container_id.as_str(),
                options,
                body_full(archive_bytes.to_vec().into()),
            )
            .await?;

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

/// Build the bollard [`Mount`] that binds the host mirrorlist into the builder
/// container at `/etc/pacman.d/mirrorlist`.
///
/// Returns `None` for non-x86_64 architectures (their mirrorlist differs from
/// the x86_64 one maintained by AURCache).
///
/// The source path comes from [`BuildMode::mirrorlist_source_path`]:
/// - An absolute path → plain bind mount.
/// - A relative path → treated as `volume_name/subpath` and expressed as a
///   named-volume mount with a subpath, which is how DinD volumes are addressed.
pub fn mirrorlist_mount(arch: &str) -> anyhow::Result<Option<Mount>> {
    if arch != "linux/x86_64" {
        return Ok(None);
    }

    const TARGET: &str = "/etc/pacman.d/mirrorlist";
    let source = get_build_mode().mirrorlist_source_path();

    let mnt = if source.starts_with('/') {
        Mount {
            target: Some(TARGET.to_string()),
            source: Some(source),
            typ: Some(MountTypeEnum::BIND),
            read_only: Some(false),
            ..Default::default()
        }
    } else {
        let (volume_name, subpath) = source
            .split_once('/')
            .ok_or_else(|| anyhow!("Mirrorlist path not containing '/': Invalid"))?;
        Mount {
            target: Some(TARGET.to_string()),
            source: Some(volume_name.to_string()),
            typ: Some(MountTypeEnum::VOLUME),
            read_only: Some(false),
            volume_options: Some(MountVolumeOptions {
                subpath: Some(subpath.to_string()),
                ..Default::default()
            }),
            ..Default::default()
        }
    };

    Ok(Some(mnt))
}
