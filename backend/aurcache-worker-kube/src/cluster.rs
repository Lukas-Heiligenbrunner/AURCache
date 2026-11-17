//! Everything the dispatcher asks of Kubernetes, as one trait.
//!
//! The lifecycle in [`executor`](crate::executor) only ever talks to a
//! [`Cluster`], so it can be driven by an in-memory implementation whose pods
//! change state on cue and whose `exec` is a pair of pipes.
//! [`KubeCluster`](crate::kube_cluster::KubeCluster) is the one over a real
//! API server.
//!
//! Pods are scoped: every method acts in the builds' namespace, on pods
//! carrying this dispatcher's label, as the implementation was set up.

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
    fn admits(&self, pod: &Pod) -> impl Future<Output = Admission> + Send;
    /// Delete one pod; one already gone is not an error.
    fn delete(&self, name: &str, grace: Duration) -> impl Future<Output = Result<()>> + Send;
    /// Delete every pod carrying this dispatcher's label; how many there were.
    fn sweep(&self) -> impl Future<Output = Result<usize>> + Send;
    /// The pod as the watch last saw it; `None` when there is none by that name.
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

/// Why a pod was not created.
#[derive(Debug)]
pub enum CreateError {
    /// Admission said no: a quota, the namespace's Pod Security level, a
    /// policy webhook.
    Refused {
        message: String,
    },
    /// A pod of that name exists already.
    Exists,
    Other(anyhow::Error),
}

/// What a dry run of a pod said.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Admission {
    Admitted,
    /// The cluster answered, and a pod created now would not be: admission
    /// said no, the namespace is not there, or the API server is failing or
    /// turning requests away.
    Refused {
        message: String,
    },
    /// The question did not get an answer; ask again.
    Unknown {
        message: String,
    },
}

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
    /// Resolves once the process is over.
    pub ended: BoxFuture<'static, ExecEnd>,
}

/// How an exec ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExecEnd {
    Succeeded,
    /// It exited non-zero; `message` is the status and the tail of its stderr.
    Failed {
        message: String,
    },
    /// The stream broke before a status arrived.
    Lost {
        message: String,
    },
}
