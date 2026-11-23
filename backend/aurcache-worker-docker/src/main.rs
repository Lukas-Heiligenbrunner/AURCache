//! The AURCache Docker worker.
//!
//! Enrolls and claims jobs exactly like `aurcache-worker` — same protocol, same
//! identity, same worker row — but builds each package in a container spawned
//! from the builder image instead of a `devtools` chroot.

use std::fs::{self, DirBuilder};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::Path;

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
            "BUILD_ARTIFACT_DIR is not set. The Docker worker binds a host directory into each \
             build container and cannot infer that path: set it to a directory the Docker \
             daemon can bind, as it sees it."
        )
    })?;

    make_private(&cfg.dirs.local);
    let identity = Identity::load_or_create(&core.data_dir)?;
    let executor = DockerExecutor::connect(cfg, identity.tag().to_string()).await?;

    // Waits out a server that is not up yet; see `enroll::enroll`.
    let client = enroll::enroll(&core, &identity, DockerExecutor::KIND).await?;

    Runner::new(core, client, executor).run().await
}

/// Make `dir` mode 0700 when it is not there, and warn when it is open to
/// others: a build can leave a setuid-root file in it, and Docker cannot bind
/// a directory `nosuid`.
fn make_private(dir: &Path) {
    let made = DirBuilder::new().recursive(true).mode(0o700).create(dir);
    match made.and_then(|()| fs::metadata(dir)) {
        Err(e) => tracing::warn!("creating {}: {e}", dir.display()),
        Ok(meta) if meta.permissions().mode() & 0o077 != 0 => tracing::warn!(
            "{} is mode {:o}: make it 0700, owned by this worker's user, so nothing \
             else can run a setuid file a build leaves there",
            dir.display(),
            meta.permissions().mode() & 0o7777
        ),
        Ok(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode(dir: &Path) -> u32 {
        fs::metadata(dir).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn the_directory_is_made_for_its_owner_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("above/builds");
        make_private(&dir);
        assert_eq!(mode(&dir), 0o700);
        // One that is there already is left as it is, only warned about.
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        make_private(&dir);
        assert_eq!(mode(&dir), 0o755);
    }
}
