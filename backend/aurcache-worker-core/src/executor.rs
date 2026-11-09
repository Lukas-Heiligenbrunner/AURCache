//! The seam between the worker protocol and how a package actually gets built.
//!
//! [`Runner`](crate::runner::Runner) owns everything that makes a worker a
//! worker — claiming within a concurrency limit, heartbeating, self-aborting a
//! build it can no longer report, and always emitting exactly one terminal
//! report. An [`Executor`] owns only the part that differs between build
//! strategies: turning one [`JobDescriptor`] into built packages.
//!
//! There is deliberately no runtime selection here. Each executor lives in its
//! own crate with its own binary, so neither has to know the other exists and
//! retiring one is a deletion rather than a refactor.

use anyhow::Result;
use aurcache_common::worker::{CompleteReport, JobDescriptor};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use crate::client::WorkerClient;
use crate::protocol::{StopWatch, Timeout};
use crate::settings::WorkerSettings;

/// A claimed job, with what the runner settled for it before handing it over.
pub struct ClaimedJob {
    pub job: JobDescriptor,
    /// The mirrorlist to build with, already resolved from what the server
    /// offered; `None` leaves the build environment's own.
    pub mirrorlist: Option<String>,
    /// How long the build may run: the shorter of this worker's limit and
    /// the job's, as they stood when it was claimed.
    pub timeout: Option<Timeout>,
    /// Tripped when the build has to stop: the server asked, or it has been
    /// out of reach past the lease.
    pub cancel: CancellationToken,
}

impl ClaimedJob {
    /// Start timing the build, which is starting now.
    #[must_use]
    pub fn watch(&self) -> StopWatch {
        StopWatch::start(self.timeout, self.cancel.clone())
    }
}

/// Builds one claimed job to completion.
pub trait Executor: Send + Sync + 'static {
    /// Stable identifier for this build strategy, reported at registration so
    /// the fleet can be told apart on the Workers page: `chroot`, `docker`.
    ///
    /// A free-form string rather than a closed enum, and deliberately so: a
    /// future executor should be able to name itself without the server needing
    /// to know about it first. The server stores and displays whatever arrives.
    ///
    /// An associated *const* rather than a method, because the two workers build
    /// their executor on opposite sides of enrollment -- the chroot worker
    /// enrolls first, the docker worker constructs first -- so the kind has to
    /// be readable without an instance.
    const KIND: &'static str;

    /// Run a job and return its terminal report.
    ///
    /// An error is reported for the build by the runner, through
    /// [`from_error`](crate::report::from_error), and written to its log.
    /// Implementations must not panic -- the runner reports a panic as an
    /// interruption, but an error carries a far better message.
    ///
    /// When the job's `cancel` trips, an implementation stops promptly and
    /// reports [`canceled`](crate::report::canceled); [`ClaimedJob::watch`]
    /// says when that, or the timeout, is.
    fn run_job(
        &self,
        client: Arc<WorkerClient>,
        job: ClaimedJob,
    ) -> impl Future<Output = Result<CompleteReport>> + Send;

    /// Whether this executor can take work right now.
    ///
    /// Always, unless an executor says otherwise. The chroot executor uses it
    /// to drain: some maintenance cannot run while builds do, and refusing to
    /// claim is the only way to reach a moment when none are. Claiming and
    /// then stalling the build would be worse -- a claimed job holds a lease
    /// the server expects progress on.
    ///
    /// Consulted before each claim, so an implementation should be cheap and
    /// is a reasonable place to do the waiting-for work itself. `claimed` is
    /// how many jobs this worker holds now, counted from the claim: a job is
    /// in flight before [`Self::run_job`] has started, and until its report
    /// is sent.
    fn ready_for_work(&self, claimed: usize) -> impl Future<Output = bool> + Send {
        let _ = claimed;
        async { true }
    }

    /// One-line summary of the executor, logged when the worker comes online,
    /// so a worker's logs say which build strategy produced its packages.
    fn describe_self(&self) -> String;

    /// Take a new resolution of this worker's settings -- the server delivered
    /// values -- and return the one actually in force.
    ///
    /// An executor keeps what it reads per job for its next one (a build
    /// already running keeps the limits it started with), applies at once
    /// what takes effect at once, and refuses what the machine cannot honour
    /// with [`WorkerSettings::refused`] -- each keeping its value in
    /// `previous`, the resolution in force until now -- so the value it runs
    /// and the value it reports cannot differ. The default keeps nothing and
    /// refuses nothing, for an executor whose settings are all the runner's
    /// own.
    fn reconfigure(
        &self,
        settings: WorkerSettings,
        previous: &WorkerSettings,
    ) -> impl Future<Output = WorkerSettings> + Send {
        let _ = previous;
        async { settings }
    }
}
