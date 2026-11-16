//! The worker's side of a build that `aurcache-build-agent` runs in a
//! container.
//!
//! The Docker worker and the Kubernetes worker both turn a claimed job into a
//! [`BuildSpec`](aurcache_build_agent::spec::BuildSpec) ([`spec`]), hand it in
//! with the source archive ([`input`]), and turn how the container ended into
//! one terminal report ([`outcome`]), so a build ends for the same reason
//! whichever of them ran it. They also say the same thing about what an
//! agent-run build does not do ([`unsupported`]), find the agent binary the
//! same way ([`agent`]), and name architectures the same way ([`arch`]).
//!
//! See `design/in-progress/container-workers.md`, "The worker's side".

pub mod agent;
pub mod arch;
pub mod input;
pub mod outcome;
pub mod spec;
pub mod unsupported;
