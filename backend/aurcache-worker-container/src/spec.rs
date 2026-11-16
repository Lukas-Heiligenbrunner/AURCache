//! A claimed job as the agent's `BuildSpec`.

use aurcache_build_agent::spec::{BuildSpec, Credential};
use aurcache_worker_core::executor::ClaimedJob;
use aurcache_worker_core::repo::RepoSection;
use std::path::PathBuf;

/// What the worker decides about a build, beside what the job says.
#[derive(Clone, Debug, PartialEq)]
pub struct SpecOptions {
    pub cpus: Option<f64>,
    pub keyserver: String,
    /// Where the build's own trees go, inside the container.
    pub scratch: PathBuf,
    pub credential: Option<Credential>,
}

/// The spec for `claimed`, with the AURCache repository section appended to
/// its `pacman.conf`.
#[must_use]
pub fn for_job(
    claimed: &ClaimedJob,
    repo: Option<&RepoSection>,
    options: SpecOptions,
) -> BuildSpec {
    // WP3 writes this.
    let _ = (claimed, repo, options);
    unimplemented!()
}
