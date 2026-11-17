//! What a job may ask for that an agent-run build does not do: a persistent
//! build directory.
//!
//! A kept tree only makes the next build of a package faster, and the setting
//! is on for every package that does not turn it off. A build in a fresh tree
//! is a correct build, so it goes ahead and the operator is told nothing: a
//! line per build would say the same thing about every one of them.

use aurcache_common::worker::JobDescriptor;

/// Leave a trace, for someone debugging this worker, that `job` asked for a
/// build directory to be kept and is built in a fresh tree.
pub fn note(job: &JobDescriptor) {
    if job.persistent_builddir {
        tracing::debug!(
            "build {} of {} asks for a persistent build directory; building in a fresh tree",
            job.build_id,
            job.pkgbase
        );
    }
}
