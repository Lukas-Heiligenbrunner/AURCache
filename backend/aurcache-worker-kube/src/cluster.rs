//! Everything the dispatcher asks of Kubernetes, as one trait.
//!
//! The lifecycle in `executor` only ever talks to a
//! [`Cluster`], so it can be driven by an in-memory implementation whose pods
//! change state on cue and whose `exec` is a pair of pipes.
//! [`KubeCluster`](crate::kube_cluster::KubeCluster) is the one over a real
//! API server.
//!
//! Every method acts in the builds' namespace that the implementation was set
//! up with. The watch behind [`Cluster::pod`] and [`Cluster::pods`] sees only
//! pods carrying this dispatcher's label; [`Cluster::sweep`] acts by label,
//! and the other methods on the pod or name that they are given.

use anyhow::Result;
use futures::future::BoxFuture;
use k8s_openapi::api::core::v1::Pod;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};

pub trait Cluster: Send + Sync + 'static {
    fn create(&self, pod: &Pod) -> impl Future<Output = Result<(), CreateError>> + Send;
    /// Whether the cluster would admit `pod` now: a server-side dry run.
    fn admits(&self, pod: &Pod) -> impl Future<Output = Result<()>> + Send;
    /// Delete one pod; one already gone is not an error.
    fn delete(&self, name: &str, grace: Duration) -> impl Future<Output = Result<()>> + Send;
    /// Delete the build pods selected by `which`; returns how many were not
    /// already being deleted.
    fn sweep(&self, which: Sweep) -> impl Future<Output = Result<usize>> + Send;
    /// The pod as the watch last saw it; `None` when the watch has seen none
    /// by that name.
    fn pod(&self, name: &str) -> Option<Arc<Pod>>;
    /// Every pod of this dispatcher, as the watch last saw them.
    fn pods(&self) -> Vec<Arc<Pod>>;
    /// A counter that moves whenever the watch changed anything.
    fn generation(&self) -> tokio::sync::watch::Receiver<u64>;
    fn exec(
        &self,
        pod: &str,
        container: &str,
        argv: &[&str],
        stdin: Stdin,
    ) -> impl Future<Output = Result<Exec>> + Send;
    /// The last `lines` lines a container wrote, from the kubelet's log.
    fn log_tail(
        &self,
        pod: &str,
        container: &str,
        lines: i64,
    ) -> impl Future<Output = Result<String>> + Send;
}

/// Which build pods a sweep deletes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sweep {
    /// Every pod in the builds' namespace that a dispatcher made, this one
    /// or any other. A dispatcher whose identity was lost leaves pods that
    /// only the next one can collect.
    Namespace,
    /// This dispatcher's own.
    Own,
}

/// Why a pod was not created.
#[derive(Debug)]
pub enum CreateError {
    /// The API server said no to this pod: a quota, the namespace's Pod
    /// Security level, a policy webhook, a pod of that name.
    Refused {
        message: String,
    },
    Other(anyhow::Error),
}

/// The shortest grace a pod is deleted with. None at all is a force
/// deletion: the pod leaves the API before its node has confirmed that its
/// containers are gone.
pub const PROMPT: Duration = Duration::from_secs(1);

/// Whether an exec's stdin is given to the caller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stdin {
    Closed,
    Piped,
}

/// A process running in a container.
pub struct Exec {
    /// `Some` for `Stdin::Piped`.
    pub stdin: Option<Pin<Box<dyn AsyncWrite + Send>>>,
    pub stdout: Pin<Box<dyn AsyncRead + Send>>,
    /// Resolves once the process is over. Dropping it ends the exec.
    pub ended: BoxFuture<'static, ExecEnd>,
}

/// How an exec ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExecEnd {
    /// The status said `Success`. Means the process exited 0, not that every
    /// byte of its output arrived: the API server can send it after a stream
    /// that was cut short, when the error stream ended empty. A reader that
    /// needs the output whole checks it otherwise.
    Succeeded,
    /// It exited non-zero, and `message` is the status and the tail of its
    /// stderr; or the stream broke before a status arrived, and `message`
    /// says how.
    Failed { message: String },
}
