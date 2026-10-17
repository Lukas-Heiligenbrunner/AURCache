//! Shapes and helpers shared across the workspace: the API types the server,
//! CLI and browser frontend all speak, plus the small pieces of behaviour that
//! would otherwise be duplicated or force a heavyweight dependency.
//!
//! Everything here is either dependency-free or behind a feature, so a wasm
//! consumer can take `default-features = false` and still get the types.

pub mod api;
pub mod build_state;
/// Filesystem helpers. Requires the `fs` feature: `std::fs` is meaningless in
/// a browser, so wasm consumers leave it off.
#[cfg(feature = "fs")]
pub mod fs;
pub mod ports;
pub mod repo;
pub mod schedule;
pub mod settings;
pub mod shell;
pub mod source;
/// Sizes and durations as configuration writes them (`40G`, `3h`).
pub mod units;
/// The version string binaries report: their crate release, plus the commit
/// when the build is not exactly that release.
pub mod version;
pub mod worker;
/// What a worker declares it can be configured with, and what it is running.
pub mod worker_config;
