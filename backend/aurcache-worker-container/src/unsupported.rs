//! What a job may ask for that an agent-run build does not do: a persistent
//! build directory. The build goes ahead in a fresh tree, and says so.

use aurcache_common::worker::JobDescriptor;
use aurcache_worker_core::client::WorkerClient;
use aurcache_worker_core::protocol::{log, report_warning};

/// Say, in the build log and to the server, what this job asked for that an
/// agent-run build does not do. Nothing, for a job that asked for none of it.
pub async fn warn(client: &WorkerClient, job: &JobDescriptor) {
    let Some(message) = message(job) else {
        return;
    };
    log(client, job.build_id, &format!("[worker] {message}\n")).await;
    report_warning(client, Some(job.build_id), &message).await;
}

/// The sentence for what `job` asks that is not done; `None` when it asks
/// for nothing of the kind.
fn message(job: &JobDescriptor) -> Option<String> {
    job.persistent_builddir.then(|| {
        format!(
            "{} asks for a persistent build directory, which this worker does not keep; \
             building in a fresh tree",
            job.pkgbase
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use aurcache_common::worker::MirrorlistOffer;

    fn job(persistent: bool) -> JobDescriptor {
        JobDescriptor {
            build_id: 1,
            pkgbase: "big".into(),
            arch: "x86_64".into(),
            build_flags: vec![],
            persistent_builddir: persistent,
            makepkg_conf: String::new(),
            pacman_conf: String::new(),
            mirrorlist: MirrorlistOffer::Nothing,
            pgp_keys: vec![],
            vcs_sources: vec![],
            expected_packages: vec![],
            timeout_secs: None,
            expected_version: String::new(),
        }
    }

    #[test]
    fn a_persistent_build_directory_is_named() {
        let said = message(&job(true)).unwrap();
        assert!(said.contains("big"), "{said}");
        assert!(said.contains("persistent build directory"), "{said}");
        assert!(said.contains("fresh tree"), "{said}");
    }

    #[test]
    fn a_job_that_asks_for_nothing_unsupported_says_nothing() {
        assert_eq!(message(&job(false)), None);
    }
}
