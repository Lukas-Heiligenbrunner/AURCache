//! Shared machinery for AURCache build workers.
//!
//! Everything that makes a process a *worker* lives here: identity, enrollment
//! over mTLS, the job protocol, and the claim/heartbeat/report loop. What it
//! does not contain is any notion of how a package is built — that is an
//! [`Executor`](executor::Executor), implemented once per build strategy in its
//! own crate.
//!
//! `aurcache-worker` is the supported executor: each package is built in a
//! `devtools` chroot. `aurcache-worker-docker` reproduces the pre-worker
//! behaviour of spawning a build container, and exists only so single-container
//! deployments keep working across the upgrade.
//!
//! See `design/implemented/remote-workers.md`.

pub mod artifacts;
/// Streaming a build's output into its log.
pub mod build_log;
pub mod client;
pub mod config;
pub mod enroll;
pub mod executor;
/// A limit on builds running at once that can change while they run.
pub mod gate;
pub mod identity;
/// A value replaced whole while it is in use.
pub mod live;
pub mod protocol;
pub mod repo;
pub mod report;
pub mod runner;
/// What a worker declares it can be configured with, and how it resolves one.
pub mod settings;

/// Log to stderr at `info`, or what `RUST_LOG` asks for: every worker binary's
/// first step.
pub fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
}
