//! The worker's side of a build that `aurcache-build-agent` runs in a
//! container.
//!
//! Whatever engine a worker gets its containers from, it turns a claimed job
//! into a [`BuildSpec`](aurcache_build_agent::spec::BuildSpec) ([`spec`]),
//! hands it in with the source archive ([`input`]), and turns how the
//! container ended into one terminal report ([`outcome`]), so a build ends
//! for the same reason wherever it ran. Here is also how such a worker finds
//! the agent binary ([`agent`]), names architectures ([`arch`]), names what it
//! creates after itself ([`tag`]), and keeps the SSH key a build uses without
//! holding it, in an ssh-agent ([`ssh`]).
//!
//! See `design/in-progress/container-workers.md`, "The worker's side".

pub mod agent;
pub mod arch;
pub mod input;
pub mod outcome;
pub mod spec;
pub mod ssh;
pub mod tag;
