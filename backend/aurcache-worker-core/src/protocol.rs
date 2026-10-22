//! Server interactions every executor performs around a build: streaming logs,
//! observing a remote cancel request, and uploading the results.

use anyhow::{Context, Result};
use aurcache_common::api::activity::Severity;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

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

/// Whether the server has asked for this build to stop. An unreachable server
/// has not: the lease, not this poll, decides when a build is abandoned.
pub async fn cancel_requested(client: &WorkerClient, build_id: i32) -> bool {
    client
        .job_status(build_id)
        .await
        .is_ok_and(|s| s.cancel_requested)
}

/// Why a running build is being stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stop {
    /// The worker abandoned it, or the server asked for it to stop.
    Canceled,
    /// It ran past the build timeout.
    TimedOut,
}

/// Decides, while a build runs, whether it has to be stopped -- the same way
/// for every executor, so a build ends for the same reason whichever one ran
/// it.
pub struct StopWatch<'a> {
    started: Instant,
    /// Seconds; `0` is no limit.
    timeout: u64,
    /// Set when this worker has given up on the build itself.
    cancel: &'a AtomicBool,
    build_id: i32,
    last_poll: Instant,
}

impl<'a> StopWatch<'a> {
    /// How often a running build is looked at: [`Self::check`] belongs on a
    /// timer of this period.
    pub const TICK: Duration = Duration::from_secs(5);

    /// How often the server is asked about a cancel. With N builds running, a
    /// round trip per tick is N requests every five seconds for no gain:
    /// cancel latency is dominated by build granularity anyway.
    const POLL: Duration = Duration::from_secs(30);

    /// Start timing a build that has just started.
    #[must_use]
    pub fn start(timeout: u64, cancel: &'a AtomicBool, build_id: i32) -> Self {
        let now = Instant::now();
        Self {
            started: now,
            timeout,
            cancel,
            build_id,
            last_poll: now,
        }
    }

    /// How long the build has been running.
    #[must_use]
    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// Whether to stop the build now, and why.
    ///
    /// A cancel wins over a timeout reached at the same tick: the build is
    /// reported as what was asked for, and the server, which has already moved
    /// a cancelled build on, would refuse a timeout report for it.
    pub async fn check(&mut self, client: &WorkerClient) -> Option<Stop> {
        // `>=`: a timeout of N means N seconds, not N+1.
        let timed_out = self.timeout > 0 && self.elapsed().as_secs() >= self.timeout;
        if self.cancel.load(Ordering::SeqCst) {
            return Some(Stop::Canceled);
        }
        if !timed_out && self.last_poll.elapsed() >= Self::POLL {
            self.last_poll = Instant::now();
            if cancel_requested(client, self.build_id).await {
                return Some(Stop::Canceled);
            }
        }
        timed_out.then_some(Stop::TimedOut)
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
