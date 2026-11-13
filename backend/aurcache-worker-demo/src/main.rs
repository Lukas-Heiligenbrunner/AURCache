//! Demo build worker: enrolls like a real worker, builds nothing.
//!
//! A long-lived process for public demo instances: it takes jobs off the
//! queue and completes them with synthetic, near-empty package archives, so
//! visitors see dependency resolution and publishing without spending any
//! build resources.

use anyhow::Result;
use aurcache_worker_core::config::CoreConfig;
use aurcache_worker_core::enroll;
use aurcache_worker_core::executor::Executor;
use aurcache_worker_core::identity::Identity;
use aurcache_worker_core::runner::Runner;
use aurcache_worker_demo::DemoExecutor;

#[tokio::main]
async fn main() -> Result<()> {
    aurcache_worker_core::init_tracing();

    let core = CoreConfig::from_env(
        aurcache_common::version::full_version(env!("CARGO_PKG_VERSION")),
        Vec::new(),
    );
    let identity = Identity::load_or_create(&core.data_dir)?;

    // Waits out a server that is not up yet; see `enroll::enroll`.
    let client = enroll::enroll(&core, &identity, DemoExecutor::KIND).await?;

    Runner::new(core, client, DemoExecutor).run().await
}
