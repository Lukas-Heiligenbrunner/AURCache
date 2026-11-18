//! The AURCache Docker worker.
//!
//! It builds each package in a container of its own, made from the builder
//! image by a Docker (or Podman) daemon, so it needs nothing of its machine
//! but a container engine. The build agent runs the build inside that
//! container: root for the step that needs it, then makepkg as an
//! unprivileged user. See `design/in-progress/container-workers.md`.

pub mod cleanup;
pub mod config;
pub mod container;
pub mod ending;
pub mod executor;
pub mod network;
/// What this executor declares it can be configured with.
pub mod settings;
pub mod workdir;
