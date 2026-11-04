//! AURCache legacy container build worker (deprecated).
//!
//! Enrolls and claims jobs exactly like `aurcache-worker` — same protocol, same
//! identity, same worker row — but builds each package in a container spawned
//! from the builder image instead of a `devtools` chroot.

use anyhow::{Result, anyhow};

use aurcache_worker_core::enroll;
use aurcache_worker_core::executor::Executor;
use aurcache_worker_core::identity::Identity;
use aurcache_worker_core::runner::Runner;
use aurcache_worker_docker::config::{self, Config};
use aurcache_worker_docker::executor::DockerExecutor;

#[tokio::main]
async fn main() -> Result<()> {
    aurcache_worker_core::init_tracing();

    let core = config::core_from_env();
    let cfg = Config::from_env(&core.settings).ok_or_else(|| {
        anyhow!(
            "BUILD_ARTIFACT_DIR is not set. The compatibility builder binds a host directory \
             into each build container and cannot infer that path. Set it to the same value \
             your previous AURCache deployment used, or run `aurcache-worker` instead."
        )
    })?;

    tracing::warn!(
        "The legacy container builder is deprecated and will be removed. It builds in a \
         reused container image rather than a clean chroot, and supports neither build \
         caches nor build credentials. See the Build Workers documentation for migrating \
         to the split server + worker setup."
    );

    let identity = Identity::load_or_create(&core.data_dir)?;
    let executor = DockerExecutor::connect(cfg).await?;

    // Waits out a server that is not up yet; see `enroll::enroll`.
    let client = enroll::enroll(&core, &identity, DockerExecutor::KIND).await?;

    Runner::new(core, client, executor).run().await
}
