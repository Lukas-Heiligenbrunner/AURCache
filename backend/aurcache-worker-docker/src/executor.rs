//! The Docker worker's executor: each job in a container of its own, made
//! from the builder image, with the build agent running the build.
//!
//! Per job: the build directory gets the agent and the job's inputs; the
//! container is created as [`container::render`] says and its output streamed
//! into the build log; once it ends, the agent's report (or the engine's word
//! when there is none) becomes the terminal report, and the packages in
//! `pkg/` are uploaded. The directory and the container are removed on every
//! path.

use anyhow::{Context, Result, anyhow, bail};
use aurcache_build_agent::layout::Work;
use aurcache_common::worker::{BuildOutcome, CompleteReport};
use aurcache_worker_container::agent;
use aurcache_worker_container::arch::{self, Platform};
use aurcache_worker_container::outcome;
use aurcache_worker_container::spec::{self, SpecOptions};
use aurcache_worker_core::build_log::BuildLog;
use aurcache_worker_core::client::WorkerClient;
use aurcache_worker_core::executor::{ClaimedJob, Executor};
use aurcache_worker_core::live::Live;
use aurcache_worker_core::protocol::{Stop, log, upload_artifacts};
use aurcache_worker_core::settings::{DEFAULT_KEYSERVER, WorkerSettings};
use bollard::Docker;
use bollard::models::ContainerCreateBody;
use bollard::query_parameters::{
    AttachContainerOptions, CreateContainerOptions, CreateImageOptions, KillContainerOptions,
    StartContainerOptions, WaitContainerOptions,
};
use futures::StreamExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::cleanup::{self, Leftover};
use crate::config::{BuildDirs, Config};
use crate::container::{self, Build};
use crate::ending::ending;
use crate::network::{self, NetworkPlan};
use crate::workdir::{self, Report};

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
    /// The agent binary each build directory gets a copy of.
    agent: PathBuf,
    /// This worker's own part of the names of its containers and build
    /// directories ([`aurcache_worker_core::identity::Identity::tag`]).
    tag: String,
}

/// How a build container's run ended, as this worker saw it.
struct Run {
    /// Why the worker stopped it, when it did.
    stop: Option<Stop>,
    elapsed: Duration,
    status: Option<i64>,
}

/// The container a job runs in, and where.
struct Target<'a> {
    name: String,
    platform: Platform,
    dirs: &'a BuildDirs,
}

impl DockerExecutor {
    /// Connect to the Docker daemon over the mounted socket, and find the
    /// agent builds are run with. `tag` names this worker in the names of
    /// its containers.
    pub async fn connect(cfg: Config, tag: String) -> Result<Self> {
        let agent = agent::locate().with_context(|| {
            format!(
                "the Docker worker runs each build with aurcache-build-agent; install it at {} \
                 or name it with {}",
                agent::INSTALLED,
                agent::VARIABLE
            )
        })?;
        let docker = Docker::connect_with_unix_defaults()?;
        docker.ping().await.map_err(|e| {
            anyhow!(
                "connection to the Docker socket failed: {e}\n\
                 The Docker worker needs /var/run/docker.sock mounted into this container. \
                 With Podman, install 'podman-docker' to provide the Docker socket."
            )
        })?;
        let network = network::resolve(
            |key| std::env::var(key).ok(),
            std::env::var("HOSTNAME").ok(),
        );
        tracing::info!("build containers will use network: {network:?}");
        tracing::info!("builds run with the agent at {}", agent.display());
        Ok(Self {
            cfg: Live::new(cfg),
            docker,
            network,
            agent,
            tag,
        })
    }

    /// Local and container-visible paths for one build's directory, named
    /// like its container: another instance may share `BUILD_ARTIFACT_DIR`,
    /// with builds of the same numbers.
    fn job_dirs(&self, cfg: &Config, build_id: i32) -> BuildDirs {
        let name = dir_name(&self.tag, build_id);
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
        platform: Platform,
    ) -> Result<()> {
        log(
            client,
            build_id,
            &format!("[worker] pulling {}\n", cfg.builder_image),
        )
        .await;
        let mut stream = self.docker.create_image(
            Some(CreateImageOptions {
                from_image: Some(cfg.builder_image.clone()),
                platform: container::platform_name(platform),
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
        let job = &claimed.job;
        // Building for the wrong architecture is worse than refusing the job.
        let Some(platform) = arch::platform(&job.arch) else {
            bail!("unknown architecture {:?}: refusing the job", job.arch);
        };
        let dirs = self.job_dirs(cfg, job.build_id);
        let target = Target {
            name: container_name(&self.tag, job.build_id),
            platform,
            dirs: &dirs,
        };

        // This worker, had it died mid-build, could have left its container
        // and its directory behind under this id. A build never goes into
        // what is left of one: root in that build may have put links there
        // for this worker to write through.
        self.remove_container(&target.name).await;
        self.clear(cfg, &target)
            .await
            .context("clearing what an earlier build left under this build's name")?;

        // Cleared on every path: any early return in `build_in` -- a failed
        // download, pull, container or upload -- would otherwise leave the
        // whole directory behind.
        let outcome = self.build_in(cfg, client, claimed, &target).await;
        if let Err(e) = self.clear(cfg, &target).await {
            tracing::warn!("{e:#}");
        }
        outcome
    }

    /// Build the job in its directory and upload what it produced.
    async fn build_in(
        &self,
        cfg: &Config,
        client: &Arc<WorkerClient>,
        claimed: &ClaimedJob,
        target: &Target<'_>,
    ) -> Result<CompleteReport> {
        let build_id = claimed.job.build_id;
        let source = client
            .source(build_id)
            .await
            .context("downloading source")?;
        let spec = spec::for_job(claimed, client.repo(), spec_options(cfg));
        workdir::prepare(&target.dirs.local, &self.agent, &spec, &source)
            .context("preparing the build directory")?;

        self.pull_image(cfg, client, build_id, target.platform)
            .await?;

        let body = container::render(&Build {
            image: &cfg.builder_image,
            host_dir: &target.dirs.host,
            tunables: cfg.tunables,
            network: &self.network,
        });
        let id = self.create(target, body).await?;
        let run = self.run_container(client, claimed, &id).await;
        self.remove_container(&id).await;
        let run = run?;

        let left = read_report(&target.dirs.local).await;
        let report = outcome::complete(run.stop, run.elapsed, ending(left, run.status));
        if report.outcome == BuildOutcome::Succeeded {
            upload_artifacts(client, build_id, &Work::new(&target.dirs.local).pkg()).await?;
        }
        Ok(report)
    }

    async fn create(&self, target: &Target<'_>, body: ContainerCreateBody) -> Result<String> {
        let created = self
            .docker
            .create_container(
                Some(CreateContainerOptions {
                    name: Some(target.name.clone()),
                    // The builder image is multi-arch. Without this the daemon
                    // creates the host's variant (containerd image store) or
                    // whichever was pulled last (classic store): an x86_64
                    // build for an aarch64 job either way.
                    platform: container::platform_name(target.platform),
                }),
                body,
            )
            .await
            .context("creating build container")?;
        Ok(created.id)
    }

    /// Start the container, stream its output, and wait for it while honouring
    /// cancellation and the build's timeout.
    async fn run_container(
        &self,
        client: &Arc<WorkerClient>,
        claimed: &ClaimedJob,
        container_id: &str,
    ) -> Result<Run> {
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
        let mut wait = self
            .docker
            .wait_container(container_id, None::<WaitContainerOptions>);
        let mut status = None;
        let stop = tokio::select! {
            next = wait.next() => {
                status = match next {
                    Some(Ok(res)) => Some(res.status_code),
                    // A non-zero exit surfaces here as an error carrying the code.
                    Some(Err(bollard::errors::Error::DockerContainerWaitError { code, .. })) => {
                        Some(code)
                    }
                    Some(Err(e)) => {
                        // The pump would otherwise stay detached against a
                        // stream nobody now drains.
                        pump.abort();
                        return Err(e).context("waiting for build container");
                    }
                    None => None,
                };
                None
            }
            stop = watch.stopped() => {
                // The agent is PID 1: killing it ends everything the build
                // started with it.
                let _ = self.docker.kill_container(
                    container_id,
                    None::<KillContainerOptions>,
                ).await;
                Some(stop)
            }
        };

        build_log.close(vec![pump], stop).await;
        Ok(Run {
            stop,
            elapsed: watch.elapsed(),
            status,
        })
    }

    async fn remove_container(&self, name_or_id: &str) {
        cleanup::remove_container(&self.docker, name_or_id).await;
    }

    /// Remove a build's directory.
    async fn clear(&self, cfg: &Config, target: &Target<'_>) -> Result<()> {
        let leftover = Leftover {
            dirs: target.dirs,
            image: &cfg.builder_image,
            platform: target.platform,
            name: format!("{}_clean", target.name),
        };
        cleanup::clear(&self.docker, &leftover)
            .await
            .with_context(|| {
                format!(
                    "could not remove the build directory {}",
                    target.dirs.local.display()
                )
            })
    }
}

/// The report the agent left in `dir`, read off the runtime.
async fn read_report(dir: &Path) -> Report {
    let dir = dir.to_path_buf();
    tokio::task::spawn_blocking(move || workdir::report(&dir))
        .await
        .unwrap_or_else(|e| Report::Unreadable(format!("reading it failed: {e}")))
}

/// The name of a build's container: this worker's tag, then the build. The
/// engine may be shared with a worker of another server, whose build ids are
/// the same numbers.
fn container_name(tag: &str, build_id: i32) -> String {
    format!("aurcache_build_{tag}_{build_id}")
}

/// Named like [`container_name`], for the same reason.
fn dir_name(tag: &str, build_id: i32) -> String {
    format!("{tag}_{build_id}")
}

/// What this worker decides about a build beside what the job says.
fn spec_options(cfg: &Config) -> SpecOptions {
    SpecOptions {
        cpus: cfg.tunables.cpus(),
        keyserver: DEFAULT_KEYSERVER.to_string(),
        scratch: PathBuf::from(container::SCRATCH),
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
        format!("docker ({})", self.cfg.current().builder_image)
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

#[cfg(test)]
mod tests {
    use super::{container_name, dir_name};

    /// Two workers on one engine and one `BUILD_ARTIFACT_DIR`, building the
    /// same build number of two servers, share neither name.
    #[test]
    fn names_are_the_workers_and_the_builds() {
        assert_eq!(
            container_name("0123456789abcdef", 7),
            "aurcache_build_0123456789abcdef_7"
        );
        assert_eq!(dir_name("0123456789abcdef", 7), "0123456789abcdef_7");
    }
}
