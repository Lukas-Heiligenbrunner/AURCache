//! A claimed job as the agent's `BuildSpec`.

use aurcache_build_agent::spec::{BuildSpec, INTERFACE, SshAccess};
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
    /// The hosts' keys, when the worker holds an SSH key for the build.
    pub ssh: Option<SshAccess>,
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

/// The spec for a job and the mirrorlist settled for it.
///
/// Nothing in it keeps a build tree: a job that asks for a persistent build
/// directory is built in a fresh one, and only a debug line says so. A kept
/// tree only makes the next build of a package faster, and the setting is on
/// for every package that does not turn it off, so a line for the operator
/// would say the same about every build.
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
        build_flags: build_flags(&job.build_flags),
        makepkg_conf: job.makepkg_conf.clone(),
        pacman_conf: append_to_pacman_conf(&job.pacman_conf, repo),
        pacman_upgrade_conf: job.pacman_conf.clone(),
        mirrorlist: mirrorlist.map(str::to_string),
        pgp_keys: job.pgp_keys.clone(),
        keyserver: options.keyserver,
        vcs_sources: job.vcs_sources.clone(),
        cpus: options.cpus,
        scratch: options.scratch,
        ssh: options.ssh,
    }
}

/// The job's makepkg flags, without the two that act on the build's own
/// system once the package is made: `--install` and `--rmdeps`, as words or
/// as letters of a bundle such as `-sri`.
///
/// The container is discarded when the build ends, so neither would change
/// anything that lasts. Both need root, which the build does not have: left
/// in, makepkg would build the whole package and then fail at them.
fn build_flags(flags: &[String]) -> Vec<String> {
    flags
        .iter()
        .filter_map(|flag| {
            if flag.starts_with("--") {
                return (!matches!(flag.as_str(), "--install" | "--rmdeps")).then(|| flag.clone());
            }
            let Some(letters) = flag.strip_prefix('-') else {
                return Some(flag.clone());
            };
            let kept: String = letters
                .chars()
                .filter(|c| !matches!(c, 'i' | 'r'))
                .collect();
            match kept.len() {
                0 if !letters.is_empty() => None,
                len if len == letters.len() => Some(flag.clone()),
                _ => Some(format!("-{kept}")),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use aurcache_common::worker::{JobVcsSource, MirrorlistOffer};

    /// What makepkg would do to the build's own system after the build is
    /// left out; everything else is passed as it came.
    #[test]
    fn flags_that_only_touch_the_discarded_system_are_left_out() {
        let flags = |given: &[&str]| {
            let given: Vec<String> = given.iter().map(ToString::to_string).collect();
            build_flags(&given)
        };
        assert_eq!(
            flags(&["--noconfirm", "--rmdeps", "--nocheck", "--install"]),
            ["--noconfirm", "--nocheck"]
        );
        assert_eq!(flags(&["-sri", "-r", "-i", "-ri", "-c"]), ["-s", "-c"]);
        assert_eq!(
            flags(&["--skippgpcheck", "-", "odd", "-f"]),
            ["--skippgpcheck", "-", "odd", "-f"]
        );
    }

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
        }
    }

    fn options() -> SpecOptions {
        SpecOptions {
            cpus: Some(1.5),
            keyserver: "hkps://keyserver.example".into(),
            scratch: "/scratch".into(),
            ssh: Some(SshAccess {
                known_hosts: "github.com ssh-ed25519 AAAA\n".into(),
            }),
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
        assert_eq!(spec.ssh, options().ssh);
        assert_eq!(spec.mirrorlist, None);
    }

    #[test]
    fn the_repository_section_is_appended_after_the_jobs_conf() {
        let section = section();
        let spec = describe(&job(), None, Some(&section), options());
        let expected = format!("[options]\nArchitecture = auto\n\n{}", section.text());
        assert_eq!(spec.pacman_conf, expected);
    }

    #[test]
    fn without_a_section_the_conf_is_the_jobs_own() {
        let spec = describe(&job(), None, None, options());
        assert_eq!(spec.pacman_conf, job().pacman_conf);
        assert_eq!(spec.pacman_upgrade_conf, job().pacman_conf);
    }

    /// The image is upgraded with the job's configuration exactly as the
    /// server sent it: the section this worker appends for the build is not
    /// in it.
    #[test]
    fn the_upgrade_conf_is_the_jobs_own_without_the_repository_section() {
        let section = section();
        let spec = describe(&job(), None, Some(&section), options());
        assert_eq!(spec.pacman_upgrade_conf, job().pacman_conf);
        assert!(spec.pacman_conf.contains(section.text()));
        assert!(!spec.pacman_upgrade_conf.contains(section.text()));
        let name = section.text().lines().next().unwrap();
        assert!(name.starts_with('['), "{name}");
        assert!(!spec.pacman_upgrade_conf.contains(name));
    }

    /// An event the spec's rendering emitted.
    struct Noted {
        level: tracing::Level,
        message: String,
    }

    /// A subscriber that keeps every event it is given.
    #[derive(Clone, Default)]
    struct Capture(std::sync::Arc<std::sync::Mutex<Vec<Noted>>>);

    #[derive(Default)]
    struct Message(String);

    impl tracing::field::Visit for Message {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            if field.name() == "message" {
                self.0 = format!("{value:?}");
            }
        }
    }

    impl tracing::Subscriber for Capture {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn event(&self, event: &tracing::Event<'_>) {
            let mut message = Message::default();
            event.record(&mut message);
            self.0.lock().unwrap().push(Noted {
                level: *event.metadata().level(),
                message: message.0,
            });
        }
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }

    /// The spec for a job, and what rendering it emitted.
    struct Described {
        spec: BuildSpec,
        events: Vec<Noted>,
    }

    fn described(job: &JobDescriptor) -> Described {
        let capture = Capture::default();
        let spec = tracing::subscriber::with_default(capture.clone(), || {
            describe(job, None, None, options())
        });
        let events = std::mem::take(&mut *capture.0.lock().unwrap());
        Described { spec, events }
    }

    /// Noted once, at debug, where someone debugging the worker looks and
    /// the operator's log does not; and built as any other job is, since
    /// nothing in what the agent is given keeps a tree.
    #[test]
    fn a_job_asking_to_keep_its_tree_is_noted_at_debug_and_built_fresh() {
        let kept = JobDescriptor {
            persistent_builddir: true,
            ..job()
        };
        let Described { spec, events } = described(&kept);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].level, tracing::Level::DEBUG);
        assert_eq!(
            events[0].message,
            "build 7 of hello asks for a persistent build directory; building in a fresh tree"
        );
        assert_eq!(spec, described(&job()).spec);
    }

    #[test]
    fn a_job_that_does_not_ask_is_not_noted() {
        assert!(described(&job()).events.is_empty());
    }
}
