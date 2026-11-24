//! The worker's side of a build that `aurcache-build-agent` runs in a
//! container.
//!
//! Whatever engine a worker gets its containers from, it turns a claimed job
//! into a [`BuildSpec`](aurcache_build_agent::spec::BuildSpec) ([`spec`]),
//! hands it in with the source archive ([`input`]), and turns how the
//! container ended into one terminal report ([`outcome`]), so a build ends
//! for the same reason wherever it ran. Here is also how such a worker
//! prepares a job ([`job`]), finds the agent binary ([`agent`]), names
//! architectures ([`arch`]), and keeps the SSH key that builds use in an
//! ssh-agent of its own ([`ssh`]).
//!
//! See `design/in-progress/container-workers.md`, "The worker's side".

pub mod agent;
pub mod arch;
pub mod input;
pub mod job;
pub mod outcome;
pub mod spec;
pub mod ssh;
