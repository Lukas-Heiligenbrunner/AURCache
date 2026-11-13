//! The legacy container executor.
//!
//! Reproduces the pre-worker build strategy: create a container from the
//! builder image, bind a shared build directory into it, run `makepkg`, stream
//! its output as build logs, then upload whatever landed in the directory.
//!
//! This is a compatibility path. It builds in a container rather than a clean
//! chroot, so a build can see whatever the image accumulated from the previous
//! `pacman -Syu`, and it offers none of the chroot worker's caches or
//! credential handling. New deployments should use `aurcache-worker`.

use anyhow::{Context, Result, anyhow};
use aurcache_common::worker::{BuildOutcome, CompleteReport};
use aurcache_worker_core::artifacts;
use aurcache_worker_core::build_log::BuildLog;
use aurcache_worker_core::client::WorkerClient;
use aurcache_worker_core::executor::{ClaimedJob, Executor};
use aurcache_worker_core::live::Live;
use aurcache_worker_core::protocol::{log, upload_artifacts};
use aurcache_worker_core::report::{self, Exit};
use aurcache_worker_core::settings::WorkerSettings;
use bollard::Docker;
use bollard::models::{ContainerCreateBody, EndpointSettings, HostConfig, NetworkingConfig};
use bollard::query_parameters::{
    AttachContainerOptions, CreateContainerOptions, CreateImageOptions, RemoveContainerOptions,
    StartContainerOptions,
};
use futures::StreamExt;
use std::path::Path;
use std::sync::Arc;

use crate::commands;
use crate::config::{BuildDirs, Config};
use crate::network::{self, NetworkPlan};

/// Where the shared build directory is mounted inside the container. The
/// server renders `PKGDEST` to this path, so makepkg writes finished packages
/// straight into the bind mount.
const CONTAINER_PKGDEST: &str = "/output";
/// Where sources are placed, inside the same bind mount.
const CONTAINER_SRC: &str = "/output/src";
/// Where the job's makepkg config is written, inside the bind mount.
const MAKEPKG_CONF_PATH: &str = "/output/makepkg.conf";
const BUILD_USER: &str = "ab";

/// Builds each package in a container spawned from the builder image.
pub struct DockerExecutor {
    /// The configuration in force. Each build takes the one current when it
    /// starts and keeps it, so a container is never created under one limit
    /// and timed by another.
    cfg: Live<Config>,
    docker: Docker,
    /// How spawned build containers are attached to the network, so the repo
    /// URL in the job's pacman.conf resolves from inside them.
    network: NetworkPlan,
}

impl DockerExecutor {
    /// Connect to the Docker daemon over the mounted socket.
    pub async fn connect(cfg: Config) -> Result<Self> {
        let docker = Docker::connect_with_unix_defaults()?;
        docker.ping().await.map_err(|e| {
            anyhow!(
                "connection to the Docker socket failed: {e}\n\
                 The compatibility builder needs /var/run/docker.sock mounted into this \
                 container. If using podman, install 'podman-docker' to provide the docker \
                 socket, or run the chroot worker instead (see the Build Workers docs)."
            )
        })?;
        let network = network::resolve(
            |key| std::env::var(key).ok(),
            std::env::var("HOSTNAME").ok(),
        );
        tracing::info!("build containers will use network: {network:?}");
        Ok(Self {
            cfg: Live::new(cfg),
            docker,
            network,
        })
    }

    /// Local and container-visible paths for one build's shared directory.
    fn job_dirs(&self, cfg: &Config, build_id: i32) -> BuildDirs {
        let name = build_id.to_string();
        BuildDirs {
            host: cfg.dirs.host.join(&name),
            local: cfg.dirs.local.join(&name),
        }
    }

    async fn pull_image(
        &self,
        cfg: &Config,
        client: &WorkerClient,
        build_id: i32,
        arch: &str,
    ) -> Result<()> {
        log(
            client,
            build_id,
            &format!("[worker] pulling {}\n", cfg.builder_image),
        )
        .await;
        let platform = docker_arch(arch)
            .with_context(|| format!("unknown architecture {arch:?}: refusing the pull"))?;
        let mut stream = self.docker.create_image(
            Some(CreateImageOptions {
                from_image: Some(cfg.builder_image.clone()),
                platform: format!("linux/{platform}"),
                ..Default::default()
            }),
            None,
            None,
        );
        let mut pull_error = None;
        while let Some(item) = stream.next().await {
            if let Err(e) = item {
                tracing::warn!("image pull reported: {e}");
                pull_error = Some(e);
                break;
            }
        }
        // A pull failure is survivable only when the image is already local.
        // Without the image, the later failure is a bare "No such image" that
        // hides the real (TLS/auth/registry) cause — so fail here, with it.
        if let Some(e) = pull_error
            && self.docker.inspect_image(&cfg.builder_image).await.is_err()
        {
            return Err(e).context(format!(
                "pulling {} failed and the image is not present locally",
                cfg.builder_image
            ));
        }
        Ok(())
    }

    async fn build(
        &self,
        cfg: &Config,
        client: &Arc<WorkerClient>,
        claimed: &ClaimedJob,
    ) -> Result<CompleteReport> {
        let dirs = self.job_dirs(cfg, claimed.job.build_id);

        // A crash mid-job could have left a tree behind under this id.
        let _ = std::fs::remove_dir_all(&dirs.local);
        let src_dir = dirs.local.join("src");
        std::fs::create_dir_all(&src_dir)
            .with_context(|| format!("creating {}", src_dir.display()))?;
        // The build container runs as its image's own unprivileged user, whose
        // uid this process cannot know and does not share. It must be able to
        // write sources, `makepkg.conf` and finished packages here, so the
        // directory is world-writable — as the pre-worker builder also made it.
        // It lives inside a per-build directory that is removed afterwards.
        world_writable(&dirs.local);
        world_writable(&src_dir);

        // Removed on every path, not just the happy one: any early return in
        // `build_in` -- a failed download, pull, container or upload -- would
        // otherwise leave the whole directory behind.
        let outcome = self.build_in(cfg, client, claimed, &dirs).await;
        let _ = std::fs::remove_dir_all(&dirs.local);
        outcome
    }

    /// Build `job` in its prepared directories and upload what it produced.
    async fn build_in(
        &self,
        cfg: &Config,
        client: &Arc<WorkerClient>,
        claimed: &ClaimedJob,
        dirs: &BuildDirs,
    ) -> Result<CompleteReport> {
        let job = &claimed.job;
        let build_id = job.build_id;
        let BuildDirs {
            host: host_dir,
            local: local_dir,
        } = dirs;
        let src_dir = local_dir.join("src");

        let source = client
            .source(build_id)
            .await
            .context("downloading source")?;
        artifacts::extract_source(&source, &src_dir).context("extracting source")?;

        self.pull_image(cfg, client, build_id, &job.arch).await?;

        // Bind-mounted rather than written in: see wrap_with_config.
        let mut binds = vec![format!("{}:{CONTAINER_PKGDEST}", host_dir.display())];
        if let Some(list) = &claimed.mirrorlist {
            let path = local_dir.join("mirrorlist");
            std::fs::write(&path, list).with_context(|| format!("writing {}", path.display()))?;
            binds.push(etc_bind(host_dir, "mirrorlist", "/etc/pacman.d/mirrorlist"));
        }

        // The server sends no `[repo]` section; append the one rendered for the
        // host this worker reaches the server on.
        let pacman_conf =
            aurcache_worker_core::repo::append_to_pacman_conf(&job.pacman_conf, client.repo());
        let path = local_dir.join("pacman.conf");
        std::fs::write(&path, &pacman_conf)
            .with_context(|| format!("writing {}", path.display()))?;
        binds.push(etc_bind(host_dir, "pacman.conf", "/etc/pacman.conf"));

        let cmd = commands::wrap_with_config(
            &commands::with_layout(&job.makepkg_conf, CONTAINER_PKGDEST),
            MAKEPKG_CONF_PATH,
            &commands::build_build_command(
                &job.pkgbase,
                &job.build_flags,
                Path::new(CONTAINER_SRC),
                MAKEPKG_CONF_PATH,
            ),
        );

        let container_name = format!("aurcache_build_{build_id}");
        let body = ContainerCreateBody {
            image: Some(cfg.builder_image.clone()),
            attach_stdout: Some(true),
            attach_stderr: Some(true),
            open_stdin: Some(false),
            user: Some(BUILD_USER.to_string()),
            cmd: Some(vec![
                "bash".to_string(),
                "-leco".to_string(),
                "pipefail".to_string(),
                cmd,
            ]),
            host_config: Some(HostConfig {
                auto_remove: Some(false),
                nano_cpus: cfg.tunables.nano_cpus(),
                // Both, and equal: `MEMORY_LIMIT` is RAM and swap together,
                // which Docker spells as a `MemorySwap` total equal to the
                // `Memory` limit -- no swap beyond it. `MemorySwap` alone is
                // refused ("you should always set the Memory limit when using
                // Memoryswap limit"), which is how this limit went unapplied.
                memory: cfg.tunables.memory_bytes(),
                memory_swap: cfg.tunables.memory_bytes(),
                binds: Some(binds),
                network_mode: self.network.network_mode(),
                ..Default::default()
            }),
            networking_config: self
                .network
                .endpoint_network()
                .map(|name| NetworkingConfig {
                    endpoints_config: Some(std::collections::HashMap::from([(
                        name.to_string(),
                        EndpointSettings::default(),
                    )])),
                }),
            ..Default::default()
        };

        let created = self
            .docker
            .create_container(
                Some(CreateContainerOptions {
                    name: Some(container_name.clone()),
                    // The builder image is multi-arch. Without this the daemon
                    // creates the host's variant (containerd image store) or
                    // whichever was pulled last (classic store): an x86_64
                    // build for an aarch64 job either way. 0.5.0 passed it.
                    // `pull_image` has refused an architecture this lacks.
                    platform: docker_arch(&job.arch)
                        .map(|platform| format!("linux/{platform}"))
                        .unwrap_or_default(),
                }),
                body,
            )
            .await
            .context("creating build container")?;

        let result = self.run_container(client, claimed, &created.id).await;

        // Always remove the container, whatever happened to the build.
        let _ = self
            .docker
            .remove_container(
                &created.id,
                Some(RemoveContainerOptions {
                    force: true,
                    ..Default::default()
                }),
            )
            .await;

        let report = result?;
        if report.outcome == BuildOutcome::Succeeded {
            upload_artifacts(client, build_id, local_dir).await?;
        }
        Ok(report)
    }

    /// Start the container, stream its output, and wait for it while honouring
    /// cancellation and the build's timeout.
    async fn run_container(
        &self,
        client: &Arc<WorkerClient>,
        claimed: &ClaimedJob,
        container_id: &str,
    ) -> Result<CompleteReport> {
        let build_id = claimed.job.build_id;
        let attached = self
            .docker
            .attach_container(
                container_id,
                Some(AttachContainerOptions {
                    stdout: true,
                    stderr: true,
                    stream: true,
                    ..Default::default()
                }),
            )
            .await
            .context("attaching to build container")?;

        self.docker
            .start_container(container_id, None::<StartContainerOptions>)
            .await
            .context("starting build container")?;

        // Pump container output into the build log.
        let build_log = BuildLog::open(Arc::clone(client), build_id);
        let mut output = attached.output;
        let tx = build_log.sender();
        let pump = tokio::spawn(async move {
            while let Some(Ok(chunk)) = output.next().await {
                if tx.send(chunk.to_string()).await.is_err() {
                    break;
                }
            }
        });

        let watch = claimed.watch();
        let mut wait = self.docker.wait_container(
            container_id,
            None::<bollard::query_parameters::WaitContainerOptions>,
        );
        // What the container said as it ended; nothing, when it was stopped.
        let mut exit = Exit::Unknown;
        let stop = tokio::select! {
            next = wait.next() => {
                exit = match next {
                    Some(Ok(res)) => Exit::Code(res.status_code),
                    // A non-zero exit surfaces here as an error carrying the code.
                    Some(Err(bollard::errors::Error::DockerContainerWaitError { code, .. })) => {
                        Exit::Code(code)
                    }
                    Some(Err(e)) => {
                        // The pump would otherwise stay detached against a
                        // stream nobody now drains.
                        pump.abort();
                        return Err(e).context("waiting for build container");
                    }
                    None => Exit::Unknown,
                };
                None
            }
            stop = watch.stopped() => {
                let _ = self.docker.kill_container(
                    container_id,
                    None::<bollard::query_parameters::KillContainerOptions>,
                ).await;
                Some(stop)
            }
        };

        build_log.close(vec![pump], stop).await;
        // The legacy container builder samples neither the build tree nor the
        // sources it was made from, so nothing is measured on either path.
        Ok(report::ended(stop, watch.elapsed(), exit))
    }
}

impl Executor for DockerExecutor {
    const KIND: &'static str = "docker";

    async fn run_job(
        &self,
        client: Arc<WorkerClient>,
        claimed: ClaimedJob,
    ) -> Result<CompleteReport> {
        // Once, here: everything this build does reads this copy.
        let cfg = self.cfg.current();
        self.build(&cfg, &client, &claimed).await
    }

    fn describe_self(&self) -> String {
        format!(
            "legacy container ({}) [deprecated]",
            self.cfg.current().builder_image
        )
    }

    /// Nothing here needs the machine's say beyond what the declaration
    /// already checks: Docker enforces whatever limit it is given. So every
    /// value is taken, for the next build.
    async fn reconfigure(
        &self,
        settings: WorkerSettings,
        _previous: &WorkerSettings,
    ) -> WorkerSettings {
        self.cfg
            .replace(self.cfg.current().with_settings(&settings));
        settings
    }
}

/// Bind the build directory's `name` over `target`, a file a package owns.
///
/// Writable, not `:ro`: with `CheckSpace` on (as in Arch's stock
/// `pacman.conf`), a `pacman -Syu` that upgrades the owning package
/// (`pacman`, `pacman-mirrorlist`) checks every mount point the transaction
/// writes to, and a single-file bind is its own mount point. A read-only one
/// fails every build with "not enough free disk space" until the builder
/// image catches up with the repositories. Writable, pacman keeps the file,
/// since it differs from the packaged one, and writes any new default beside
/// it as `.pacnew`.
///
/// This exposes nothing: the container already reaches the same file through
/// the build directory at [`CONTAINER_PKGDEST`], which is removed after the
/// build.
fn etc_bind(host_dir: &Path, name: &str, target: &str) -> String {
    format!("{}/{name}:{target}", host_dir.display())
}

/// Best-effort `chmod 0777`. A failure is logged here; the build then fails on
/// the write it could not make, with its own error.
fn world_writable(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o777)) {
            tracing::warn!("could not make {} writable: {e}", path.display());
        }
    }
}

/// Map an Arch architecture name onto Docker's platform vocabulary.
///
/// `None` for anything unrecognised: pulling the amd64 image for a job that
/// is not amd64 builds the wrong thing (or fails confusingly), which is
/// worse than refusing the job with a clear error.
fn docker_arch(arch: &str) -> Option<&'static str> {
    Some(match arch {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "armv7h" => "arm/v7",
        "riscv64" => "riscv64",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_arch_names_to_docker_platforms() {
        assert_eq!(docker_arch("x86_64"), Some("amd64"));
        assert_eq!(docker_arch("aarch64"), Some("arm64"));
        assert_eq!(docker_arch("armv7h"), Some("arm/v7"));
        assert_eq!(docker_arch("riscv64"), Some("riscv64"));
    }

    /// A read-only bind over a packaged file fails every `pacman -Syu` that
    /// upgrades its package.
    #[test]
    fn etc_binds_are_writable() {
        assert_eq!(
            etc_bind(Path::new("/srv/build/7"), "pacman.conf", "/etc/pacman.conf"),
            "/srv/build/7/pacman.conf:/etc/pacman.conf"
        );
    }

    /// An arch nobody taught the mapper must refuse the job, not silently
    /// pull amd64 and build the wrong thing.
    #[test]
    fn unknown_arches_map_to_nothing() {
        assert_eq!(docker_arch("sparc"), None);
        assert_eq!(docker_arch(""), None);
    }
}
