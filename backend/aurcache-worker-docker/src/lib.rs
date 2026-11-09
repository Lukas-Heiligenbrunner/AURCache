//! AURCache legacy container build worker.
//!
//! **Deprecated.** This exists so that deployments predating the remote-worker
//! architecture keep building after an upgrade, by reproducing the old
//! strategy of spawning a build container per package against the host's
//! Docker socket. It is selected only by the hybrid compatibility image, and
//! only when `BUILD_ARTIFACT_DIR` shows the deployment was previously using
//! host build mode.
//!
//! New deployments should run `aurcache-worker`, which builds in a clean
//! `devtools` chroot and supports source/package caches and build credentials.

pub mod commands;
pub mod config;
pub mod executor;
pub mod network;
/// What this executor declares it can be configured with.
pub mod settings;
