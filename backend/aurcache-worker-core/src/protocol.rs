//! Server interactions every executor performs around a build: logging,
//! deciding when it has to stop, and uploading the results.

use anyhow::{Context, Result};
use aurcache_common::api::activity::Severity;
use aurcache_common::worker::JobDescriptor;
use std::path::Path;
use std::time::Duration;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::artifacts;
use crate::client::WorkerClient;

/// The build's packages cannot be accepted: it made none, one is not a
/// regular file, or the server refused one (too large, a bad name, empty).
///
/// The package's doing rather than the worker's, so the build fails instead
/// of being retried; see [`crate::report::from_error`].
#[derive(Debug)]
pub struct PackagesRefused(pub String);

impl std::fmt::Display for PackagesRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for PackagesRefused {}

/// Append a log line, ignoring transport errors.
///
/// Log delivery is best-effort by design: losing a line is a cosmetic problem,
/// while failing the build over one would turn a blip in the log channel into a
/// lost build.
pub async fn log(client: &WorkerClient, build_id: i32, text: &str) {
    if let Err(e) = client.append_log(build_id, text).await {
        tracing::debug!("log append failed: {e}");
    }
}

/// Tell the server about a problem, same best-effort delivery as [`log`]: this
/// is what lets it show up on the Logs page and the worker's own page instead
/// of only in this process's own journal, but it is never worth failing a
/// build over losing one.
pub async fn report_warning(client: &WorkerClient, build_id: Option<i32>, message: &str) {
    if let Err(e) = client
        .report_problem(Severity::Warning, build_id, message)
        .await
    {
        tracing::debug!("worker warning report failed: {e}");
    }
}

/// Why a running build is being stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stop {
    /// The worker abandoned it, or the server asked for it to stop.
    Canceled,
    /// It ran past this limit.
    TimedOut(Timeout),
}

/// How long a build may run, and whose limit that is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timeout {
    pub limit: Duration,
    pub set_by: TimeoutSource,
}

/// Where a build's [`Timeout`] came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimeoutSource {
    /// This worker's `build_timeout`.
    Worker,
    /// The server's job timeout for the package, sent with the job.
    Server,
}

impl Timeout {
    /// The shorter of this worker's build timeout and the one the server
    /// sent with the job, or none when neither sets one. `0` from the server
    /// is no limit.
    #[must_use]
    pub fn for_job(own: Option<Duration>, job: &JobDescriptor) -> Option<Self> {
        let own = own.map(|limit| Self {
            limit,
            set_by: TimeoutSource::Worker,
        });
        let server = job.timeout_secs.filter(|&secs| secs > 0).map(|secs| Self {
            limit: Duration::from_secs(secs),
            set_by: TimeoutSource::Server,
        });
        match (own, server) {
            (Some(own), Some(server)) if server.limit < own.limit => Some(server),
            (own, server) => own.or(server),
        }
    }
}

/// Decides, while a build runs, when it has to be stopped -- the same way for
/// every executor, so a build ends for the same reason whichever one ran it.
///
/// A cancel arrives through the token: the runner trips it when the
/// heartbeat's answer names the build, or when the server has been out of
/// reach past the lease.
pub struct StopWatch {
    started: Instant,
    timeout: Option<Timeout>,
    cancel: CancellationToken,
}

impl StopWatch {
    /// Start timing a build that has just started.
    #[must_use]
    pub fn start(timeout: Option<Timeout>, cancel: CancellationToken) -> Self {
        Self {
            started: Instant::now(),
            timeout,
            cancel,
        }
    }

    /// How long the build has been running.
    #[must_use]
    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// Resolves when the build has to stop, with why; never, for a build
    /// with no limit that nobody cancels.
    ///
    /// A cancel wins over a timeout reached at the same moment: the build is
    /// reported as what was asked for, and the server, which has already
    /// moved a cancelled build on, would refuse a timeout report for it.
    pub async fn stopped(&self) -> Stop {
        let timed_out = async {
            match self.timeout {
                Some(timeout) => {
                    tokio::time::sleep_until(self.started + timeout.limit).await;
                    Stop::TimedOut(timeout)
                }
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            biased;
            () = self.cancel.cancelled() => Stop::Canceled,
            stop = timed_out => stop,
        }
    }
}

/// Upload every built artifact to the server's staging area.
///
/// Artifacts are streamed from disk rather than read into memory: package
/// files can be multi-gigabyte, and buffering them fully would triple the
/// worker's peak memory (one copy in the filesystem cache, one in the read,
/// one in the request body).
pub async fn upload_artifacts(client: &WorkerClient, build_id: i32, pkgdir: &Path) -> Result<()> {
    let found = artifacts::discover_artifacts(pkgdir);
    if found.is_empty() {
        return Err(PackagesRefused("build produced no artifacts".into()).into());
    }
    for path in found {
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .context("artifact has no filename")?
            .to_string();
        // `O_NOFOLLOW`, and a regular file: see `discover_artifacts`. Checked
        // again at the open, which is the moment that decides what is read.
        let reader = match tokio::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
            .await
        {
            Ok(reader) => reader,
            Err(e) if e.raw_os_error() == Some(libc::ELOOP) => {
                return Err(PackagesRefused(format!(
                    "{} is a symlink; refusing to upload it",
                    path.display()
                ))
                .into());
            }
            Err(e) => {
                return Err(e).with_context(|| format!("opening {}", path.display()));
            }
        };
        let meta = reader
            .metadata()
            .await
            .with_context(|| format!("reading {}", path.display()))?;
        if !meta.is_file() {
            return Err(PackagesRefused(format!(
                "{} is not a regular file; refusing to upload it",
                path.display()
            ))
            .into());
        }
        let len = Some(meta.len());
        log(client, build_id, &format!("[worker] uploading {name}\n")).await;
        client
            .upload_artifact(build_id, &name, reader, len)
            .await
            .with_context(|| format!("uploading {name}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use aurcache_common::worker::MirrorlistOffer;

    fn job(timeout_secs: Option<u64>) -> JobDescriptor {
        JobDescriptor {
            build_id: 1,
            pkgbase: "hello".into(),
            arch: "x86_64".into(),
            build_flags: Vec::new(),
            persistent_builddir: false,
            makepkg_conf: String::new(),
            pacman_conf: String::new(),
            mirrorlist: MirrorlistOffer::Nothing,
            pgp_keys: Vec::new(),
            vcs_sources: Vec::new(),
            expected_packages: vec!["hello".into()],
            timeout_secs,
            expected_version: "1-1".into(),
        }
    }

    const fn limit(secs: u64, set_by: TimeoutSource) -> Timeout {
        Timeout {
            limit: Duration::from_secs(secs),
            set_by,
        }
    }

    const fn secs(secs: u64) -> Duration {
        Duration::from_secs(secs)
    }

    #[test]
    fn the_shorter_limit_wins_whichever_side_sets_it() {
        use TimeoutSource::{Server, Worker};
        assert_eq!(
            Timeout::for_job(Some(secs(3 * 3600)), &job(Some(3600))),
            Some(limit(3600, Server))
        );
        assert_eq!(
            Timeout::for_job(Some(secs(1800)), &job(Some(3600))),
            Some(limit(1800, Worker))
        );
    }

    /// No limit from one side leaves the other's; `0` from the server is no
    /// limit, not an instant one.
    #[test]
    fn no_limit_from_one_side_leaves_the_other() {
        use TimeoutSource::{Server, Worker};
        assert_eq!(
            Timeout::for_job(None, &job(Some(3600))),
            Some(limit(3600, Server))
        );
        assert_eq!(
            Timeout::for_job(Some(secs(1800)), &job(Some(0))),
            Some(limit(1800, Worker))
        );
        assert_eq!(
            Timeout::for_job(Some(secs(1800)), &job(None)),
            Some(limit(1800, Worker))
        );
        assert_eq!(Timeout::for_job(None, &job(None)), None);
    }

    /// The limit is the limit: not a tick later.
    #[tokio::test(start_paused = true)]
    async fn a_build_stops_when_its_limit_is_reached() {
        let timeout = limit(90, TimeoutSource::Server);
        let watch = StopWatch::start(Some(timeout), CancellationToken::new());
        assert_eq!(watch.stopped().await, Stop::TimedOut(timeout));
        assert_eq!(watch.elapsed(), Duration::from_secs(90));
    }

    /// A cancel lands at once, and wins over a limit already reached.
    #[tokio::test(start_paused = true)]
    async fn a_cancel_stops_the_build_and_wins_over_the_limit() {
        let cancel = CancellationToken::new();
        let watch = StopWatch::start(Some(limit(60, TimeoutSource::Worker)), cancel.clone());
        tokio::time::advance(Duration::from_secs(10)).await;
        cancel.cancel();
        assert_eq!(watch.stopped().await, Stop::Canceled);
        assert_eq!(watch.elapsed(), Duration::from_secs(10));

        tokio::time::advance(Duration::from_secs(60)).await;
        assert_eq!(watch.stopped().await, Stop::Canceled);
    }
}
