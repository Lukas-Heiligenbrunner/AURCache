//! A claimed job as the agent's `BuildSpec`.

use aurcache_build_agent::spec::{BuildSpec, INTERFACE};
use aurcache_common::worker::JobDescriptor;
use aurcache_worker_core::executor::ClaimedJob;
use aurcache_worker_core::repo::{RepoSection, append_to_pacman_conf};
use std::path::PathBuf;

/// What the worker decides about a build, beside what the job says.
#[derive(Clone, Debug, PartialEq)]
pub struct SpecOptions {
    pub cpus: Option<f64>,
    pub keyserver: String,
    /// Where the build's own trees go, inside the container.
    pub scratch: PathBuf,
}

/// The spec for `claimed`, with the AURCache repository section appended to
/// its `pacman.conf`.
#[must_use]
pub fn for_job(
    claimed: &ClaimedJob,
    repo: Option<&RepoSection>,
    options: SpecOptions,
) -> BuildSpec {
    describe(&claimed.job, claimed.mirrorlist.as_deref(), repo, options)
}

/// The spec for a job and the mirrorlist settled for it, always in a fresh build tree.
fn describe(
    job: &JobDescriptor,
    mirrorlist: Option<&str>,
    repo: Option<&RepoSection>,
    options: SpecOptions,
) -> BuildSpec {
    if job.persistent_builddir {
        tracing::debug!(
            "build {} of {} asks for a persistent build directory; building in a fresh tree",
            job.build_id,
            job.pkgbase
        );
    }
    BuildSpec {
        interface: INTERFACE,
        pkgbase: job.pkgbase.clone(),
        arch: job.arch.clone(),
        build_flags: job.build_flags.clone(),
        makepkg_conf: job.makepkg_conf.clone(),
        pacman_conf: append_to_pacman_conf(&job.pacman_conf, repo),
        pacman_upgrade_conf: job.pacman_conf.clone(),
        mirrorlist: mirrorlist.map(str::to_string),
        pgp_keys: job.pgp_keys.clone(),
        keyserver: options.keyserver,
        vcs_sources: job.vcs_sources.clone(),
        cpus: options.cpus,
        scratch: options.scratch,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aurcache_common::worker::{JobVcsSource, MirrorlistOffer};

    fn job() -> JobDescriptor {
        JobDescriptor {
            build_id: 7,
            pkgbase: "hello".into(),
            arch: "x86_64".into(),
            build_flags: vec!["--nocheck".into(), "--skippgpcheck".into()],
            persistent_builddir: false,
            makepkg_conf: "PACKAGER=\"me\"\n".into(),
            pacman_conf: "[options]\nArchitecture = auto".into(),
            mirrorlist: MirrorlistOffer::Nothing,
            pgp_keys: vec!["ABCD".into()],
            vcs_sources: vec![JobVcsSource {
                source_url: "git+https://example.org/x.git#branch=main".into(),
                dir: "x".into(),
                git_ref: "main".into(),
            }],
            expected_packages: vec!["hello".into()],
            timeout_secs: None,
            expected_version: String::new(),
            ssh_key: None,
        }
    }

    fn options() -> SpecOptions {
        SpecOptions {
            cpus: Some(1.5),
            keyserver: "hkps://keyserver.example".into(),
            scratch: "/scratch".into(),
        }
    }

    fn section() -> RepoSection {
        RepoSection::parse("[aurcache]\nServer = http://aur.example:8081/$arch\n".into()).unwrap()
    }

    #[test]
    fn it_carries_what_the_agent_needs_from_the_job() {
        let job = job();
        let spec = describe(&job, Some("Server = https://m/$repo"), None, options());
        assert_eq!(spec.interface, INTERFACE);
        assert_eq!(spec.pkgbase, job.pkgbase);
        assert_eq!(spec.arch, job.arch);
        assert_eq!(spec.build_flags, job.build_flags);
        assert_eq!(spec.makepkg_conf, job.makepkg_conf);
        assert_eq!(spec.mirrorlist.as_deref(), Some("Server = https://m/$repo"));
        assert_eq!(spec.pgp_keys, job.pgp_keys);
        assert_eq!(spec.vcs_sources, job.vcs_sources);
    }

    #[test]
    fn it_carries_what_the_worker_decided() {
        let spec = describe(&job(), None, None, options());
        assert_eq!(spec.cpus, Some(1.5));
        assert_eq!(spec.keyserver, "hkps://keyserver.example");
        assert_eq!(spec.scratch, PathBuf::from("/scratch"));
        assert_eq!(spec.mirrorlist, None);
    }

    /// The section is appended for the build; the image is upgraded with the
    /// job's configuration exactly as the server sent it.
    #[test]
    fn the_repository_section_is_in_the_builds_conf_only() {
        let section = section();
        let spec = describe(&job(), None, Some(&section), options());
        let expected = format!("[options]\nArchitecture = auto\n\n{}", section.text());
        assert_eq!(spec.pacman_conf, expected);
        assert_eq!(spec.pacman_upgrade_conf, job().pacman_conf);
        let without = describe(&job(), None, None, options());
        assert_eq!(without.pacman_conf, job().pacman_conf);
    }
}
