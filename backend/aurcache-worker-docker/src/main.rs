//! The AURCache Docker worker.
//!
//! Enrolls and claims jobs exactly like `aurcache-worker` — same protocol, same
//! identity, same worker row — but builds each package in a container spawned
//! from the builder image instead of a `devtools` chroot.
//!
//! Told to stop (SIGTERM, SIGINT), it claims nothing more, stops the builds
//! it holds, which removes their containers and directories, and reports
//! each to the server as interrupted.

use anyhow::{Result, anyhow};
use std::time::Duration;

use aurcache_worker_container::tag;
use aurcache_worker_core::enroll;
use aurcache_worker_core::executor::Executor;
use aurcache_worker_core::identity::Identity;
use aurcache_worker_core::runner::Runner;
use aurcache_worker_core::shutdown::{self, StopSignals};
use aurcache_worker_docker::config::{self, Config};
use aurcache_worker_docker::executor::DockerExecutor;

/// How long the builds held when this worker is told to stop get to be
/// stopped, cleaned up after and reported. `docker stop` waits ten seconds
/// before it kills, and a service manager longer.
const WIND_DOWN: Duration = Duration::from_secs(30);

#[tokio::main]
async fn main() -> Result<()> {
    aurcache_worker_core::init_tracing();

    let core = config::core_from_env();
    aurcache_worker_container::arch::check(&core.arches)?;
    let cfg = Config::from_env(&core.settings)?.ok_or_else(|| {
        anyhow!(
            "BUILD_ARTIFACT_DIR is not set. The Docker worker binds a host directory into each \
             build container and cannot infer that path: set it to a directory the Docker \
             daemon can bind, as it sees it."
        )
    })?;

    if let Some(files) = &cfg.credential {
        // Once now, so that a key ssh-agent will not take stops the worker
        // here and not its first build.
        let credential = files.load().await?;
        tracing::info!(
            "Builds fetch their sources with the SSH key {} ({}). It stays with this worker, \
             in an ssh-agent started for each build: a build can have it sign while it runs, \
             and never holds it",
            files.key.display(),
            credential.agent.identity()
        );
    }

    let identity = Identity::load_or_create(&core.data_dir)?;
    let executor = DockerExecutor::connect(cfg, tag::of(&identity.fingerprint)?).await?;

    // Waits out a server that is not up yet; see `enroll::enroll`.
    let client = enroll::enroll(&core, &identity, DockerExecutor::KIND).await?;

    // From here on, and not before: until something listens, a stop signal
    // ends the process by itself, which is right while it holds no build.
    let mut signals = StopSignals::listen()?;
    let runner = Runner::new(core, client, executor);
    shutdown::run_until_told_to_stop(runner, &mut signals, WIND_DOWN).await?;
    Ok(())
}
