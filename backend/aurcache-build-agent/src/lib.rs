//! The build agent's interface, shared by the agent and the workers that run
//! it.
//!
//! A worker puts the `aurcache-build-agent` binary in a build container, gives
//! it a directory holding a [`spec::BuildSpec`] and the source archive, and
//! reads back the packages and a [`report::BuildReport`]. The types and the
//! directory layout ([`layout`]) are defined here once, so the two ends cannot
//! disagree about a field or a path. The agent and its worker may be different
//! builds, so the spec and the report carry [`spec::INTERFACE`], and the JSON
//! of each is frozen as fixtures under `tests/fixtures/`.
//!
//! The `makepkg.conf` the build runs with ([`makepkg_conf`]) lives here too,
//! where it is tested without a container. Which dependencies a build
//! installs is `aurcache_common::build_deps`, shared with the chroot worker.
//!
//! See `design/in-progress/container-workers.md`.

pub mod layout;
pub mod makepkg_conf;
pub mod report;
pub mod spec;
