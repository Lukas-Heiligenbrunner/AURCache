//! The AURCache Docker worker.
//!
//! It builds each package in a container of its own, made from a builder
//! image, against a Docker or Podman socket. The hybrid image embeds it.
//!
//! `aurcache-worker` builds in a clean `devtools` chroot instead, and supports
//! source and package caches and build credentials.

pub mod commands;
pub mod config;
pub mod executor;
pub mod network;
/// What this executor declares it can be configured with.
pub mod settings;
