//! Demo build worker: enrolls like a real worker, builds nothing.
//!
//! A long-lived process for public demo instances: it takes jobs off the
//! queue and completes them with synthetic, near-empty package archives, so
//! visitors see dependency resolution and publishing without spending any
//! build resources.

use std::sync::Arc;

use anyhow::Result;
use aurcache_worker_core::config::CoreConfig;
use aurcache_worker_core::enroll;
use aurcache_worker_core::executor::Executor;
use aurcache_worker_core::identity::Identity;
use aurcache_worker_core::runner::{Registration, Runner};
use aurcache_worker_demo::DemoExecutor;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let core = Arc::new(CoreConfig::from_env());
    let identity = Identity::load_or_create(&core.data_dir)?;

    // Waits out a server that is not up yet; see `enroll::enroll`.
    let client = enroll::enroll(&core, &identity, DemoExecutor::KIND).await?;

    let executor = Arc::new(DemoExecutor::new());
    // What the runner registers again with when delivered values change what
    // the server schedules this worker by.
    let registration = Registration {
        csr_pem: identity.generate_csr(&core.name)?,
        kind: DemoExecutor::KIND,
    };
    let runner = Runner::new(core, Arc::new(client), executor, registration);
    runner.run().await
}
