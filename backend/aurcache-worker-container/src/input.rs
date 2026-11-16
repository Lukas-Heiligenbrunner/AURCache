//! The job handed in: the spec and the source archive, as the agent reads
//! them.

use anyhow::Result;
use aurcache_build_agent::spec::BuildSpec;
use std::path::Path;

/// `<work>/in/spec.json` and `<work>/in/source.tar.gz`, for a worker that shares a directory.
pub fn write(work: &Path, spec: &BuildSpec, source: &[u8]) -> Result<()> {
    // WP3 writes this.
    let _ = (work, spec, source);
    unimplemented!()
}

/// The same two as the tar `receive` reads.
pub fn pack(spec: &BuildSpec, source: &[u8]) -> Result<Vec<u8>> {
    // WP3 writes this.
    let _ = (spec, source);
    unimplemented!()
}
