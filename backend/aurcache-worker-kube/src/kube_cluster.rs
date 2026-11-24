//! [`Cluster`] over a real API server, through kube-rs.
//!
//! One watch on the builds' namespace, filtered by this dispatcher's label,
//! feeds a reflector store, and through it [`Cluster::pod`],
//! [`Cluster::pods`] and [`Cluster::generation`]; everything else is one API
//! call.
//!
//! kube-rs stops reading an exec's stream while a pipe is full, which keeps
//! stdout to the caller's pace but would let a full stderr stop it, so stderr
//! is drained by a task of its own.

use crate::cluster::{Cluster, CreateError, Exec, ExecEnd, PROMPT, Stdin, Sweep};
use crate::pod;
use anyhow::{Context, Result};
use futures::future::BoxFuture;
use futures::{FutureExt, StreamExt};
use k8s_openapi::api::core::v1::Pod;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Status as ExitStatus;
use kube::api::{
    Api, AttachParams, AttachedProcess, DeleteParams, ListParams, LogParams, PostParams,
};
use kube::core::Status;
use kube::runtime::WatchStreamExt;
use kube::runtime::reflector::store::Writer;
use kube::runtime::reflector::{ObjectRef, Store};
use kube::runtime::watcher::{self, Event};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt, DuplexStream, SimplexStream, WriteHalf};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_util::task::AbortOnDropHandle;

/// The most an exec's stdin, stdout or stderr pipe holds: how far the caller
/// may fall behind the process before the process waits.
const PIPE: usize = 64 * 1024;
/// How much of an exec's stderr a failure message carries: its last bytes.
const STDERR_TAIL: usize = 4 * 1024;
/// How much of an exec's stderr is read at once.
const STDERR_CHUNK: usize = 8 * 1024;

/// The builds' namespace through one client, and the watch on this
/// dispatcher's pods in it.
pub struct KubeCluster {
    pods: Api<Pod>,
    namespace: String,
    /// The label selector of this dispatcher's pods.
    selector: String,
    store: Store<Pod>,
    generation: watch::Receiver<u64>,
    /// The task that keeps the store current.
    _watch: AbortOnDropHandle<()>,
}

impl KubeCluster {
    /// Connect (in-cluster, or `KUBECONFIG`), start the watch on this
    /// dispatcher's pods, and return once it has listed them.
    pub async fn connect(namespace: &str, dispatcher: &str) -> Result<Self> {
        // An error only says that a provider is already installed.
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let client = kube::Client::try_default()
            .await
            .context("connecting to the Kubernetes API")?;
        let pods: Api<Pod> = Api::namespaced(client, namespace);
        let selector = pod::selector(dispatcher);
        // The watch retries a refused list for ever; one plain list first
        // turns a missing list permission into an error here instead.
        pods.list(&ListParams::default().labels(&selector).limit(1))
            .await
            .with_context(|| format!("listing pods in namespace {namespace}"))?;
        let writer = Writer::default();
        let store = writer.as_reader();
        let (moved, generation) = watch::channel(0);
        let task = tokio::spawn(follow(pods.clone(), selector.clone(), writer, moved));
        let task = AbortOnDropHandle::new(task);
        store
            .wait_until_ready()
            .await
            .context("the watch on pods stopped before it listed them")?;
        Ok(Self {
            pods,
            namespace: namespace.to_owned(),
            selector,
            store,
            generation,
            _watch: task,
        })
    }
}

impl Cluster for KubeCluster {
    async fn create(&self, pod: &Pod) -> Result<(), CreateError> {
        match self.pods.create(&PostParams::default(), pod).await {
            Ok(_) => Ok(()),
            Err(kube::Error::Api(status)) if matches!(status.code, 400 | 403 | 409 | 422) => {
                Err(CreateError::Refused {
                    message: describe(&status),
                })
            }
            Err(err) => Err(CreateError::Other(
                anyhow::Error::new(err).context("creating a pod"),
            )),
        }
    }

    async fn admits(&self, pod: &Pod) -> Result<()> {
        let params = PostParams {
            dry_run: true,
            ..PostParams::default()
        };
        match self.pods.create(&params, pod).await {
            Ok(_) => Ok(()),
            Err(kube::Error::Api(status)) => anyhow::bail!("{}", describe(&status)),
            Err(err) => Err(anyhow::Error::new(err).context("a dry run of a pod")),
        }
    }

    async fn delete(&self, name: &str, grace: Duration) -> Result<()> {
        match self.pods.delete(name, &deleting(grace)).await {
            Ok(_) => Ok(()),
            Err(kube::Error::Api(status)) if status.is_not_found() => Ok(()),
            Err(err) => Err(err).with_context(|| format!("deleting pod {name}")),
        }
    }

    async fn sweep(&self, which: Sweep) -> Result<usize> {
        let selector = match which {
            Sweep::Namespace => pod::managed_selector(),
            Sweep::Own => self.selector.clone(),
        };
        let list = ListParams::default().labels(&selector);
        let deleted = self
            .pods
            .delete_collection(&deleting(PROMPT), &list)
            .await
            .with_context(|| format!("deleting the pods labelled {selector}"))?;
        // The API answers a collection delete with the pods it listed to
        // delete, as they were before it: one that was already being
        // deleted, by an earlier sweep or by its build, is not counted
        // again. A bare status names no pods.
        Ok(deleted.left().map_or(0, |pods| {
            pods.items
                .iter()
                .filter(|pod| pod.metadata.deletion_timestamp.is_none())
                .count()
        }))
    }

    fn pod(&self, name: &str) -> Option<Arc<Pod>> {
        self.store
            .get(&ObjectRef::new(name).within(&self.namespace))
    }

    fn pods(&self) -> Vec<Arc<Pod>> {
        self.store.state()
    }

    fn generation(&self) -> watch::Receiver<u64> {
        self.generation.clone()
    }

    async fn exec(&self, pod: &str, container: &str, argv: &[&str], stdin: Stdin) -> Result<Exec> {
        let params = AttachParams {
            container: Some(container.to_owned()),
            stdin: stdin == Stdin::Piped,
            stdout: true,
            stderr: true,
            tty: false,
            max_stdin_buf_size: Some(PIPE),
            max_stdout_buf_size: Some(PIPE),
            max_stderr_buf_size: Some(PIPE),
        };
        let mut process = self
            .pods
            .exec(pod, argv.to_vec(), &params)
            .await
            .with_context(|| format!("running {argv:?} in {pod}/{container}"))?;
        let sink = match stdin {
            Stdin::Piped => Some(process.stdin().context("the exec has no stdin")?),
            Stdin::Closed => None,
        };
        let stdout = process.stdout().context("the exec has no stdout")?;
        let stderr = process.stderr().context("the exec has no stderr")?;
        let status = process.take_status().context("the exec has no status")?;
        let (settled, is_settled) = watch::channel(false);
        let status = tokio::spawn(settle(Box::pin(status), settled));
        let stderr = tokio::spawn(stderr_tail(forward(stderr, is_settled.clone())));
        let stdin = sink.map(|sink| {
            Box::pin(feed(sink, is_settled.clone())) as Pin<Box<dyn tokio::io::AsyncWrite + Send>>
        });
        Ok(Exec {
            stdin,
            stdout: Box::pin(forward(stdout, is_settled)),
            ended: Box::pin(ended(process, status, stderr)),
        })
    }

    async fn log_tail(&self, pod: &str, container: &str, lines: i64) -> Result<String> {
        let params = LogParams {
            container: Some(container.to_owned()),
            tail_lines: Some(lines),
            ..LogParams::default()
        };
        self.pods
            .logs(pod, &params)
            .await
            .with_context(|| format!("reading the log of {pod}/{container}"))
    }
}

/// Keep `writer` current with this dispatcher's pods, moving `generation`
/// whenever that changed what the store holds. kube's watcher recovers from
/// errors itself, so its stream never ends.
async fn follow(
    pods: Api<Pod>,
    selector: String,
    writer: Writer<Pod>,
    generation: watch::Sender<u64>,
) {
    let config = watcher::Config::default().labels(&selector);
    let mut events = watcher::watcher(pods, config)
        .default_backoff()
        .reflect(writer)
        .boxed();
    while let Some(event) = events.next().await {
        match event {
            Ok(event) if changes_store(&event) => {
                generation.send_modify(|count| *count = count.wrapping_add(1));
            }
            Ok(_) => {}
            Err(err) => tracing::warn!("watching pods labelled {selector}: {err}"),
        }
    }
}

/// Whether applying `event` changed what the store holds. A relist only
/// replaces the store when it is complete.
fn changes_store(event: &Event<Pod>) -> bool {
    matches!(event, Event::Apply(_) | Event::Delete(_) | Event::InitDone)
}

fn deleting(grace: Duration) -> DeleteParams {
    DeleteParams {
        grace_period_seconds: Some(u32::try_from(grace.as_secs()).unwrap_or(u32::MAX)),
        ..DeleteParams::default()
    }
}

/// A status's message, or its reason when it has none.
fn describe(status: &Status) -> String {
    if !status.message.is_empty() {
        status.message.clone()
    } else if !status.reason.is_empty() {
        status.reason.clone()
    } else {
        format!("refused with HTTP {}", status.code)
    }
}

/// Wait for the exit status, and say on `settled` that the streams are over.
async fn settle(
    status: BoxFuture<'static, Option<ExitStatus>>,
    settled: watch::Sender<bool>,
) -> Option<ExitStatus> {
    let status = status.await;
    settled.send_replace(true);
    status
}

/// Resolves once `settled` says the streams are over.
async fn over(mut settled: watch::Receiver<bool>) {
    let _ = settled.wait_for(|settled| *settled).await;
}

/// A pipe that carries what `pipe` does, and ends where `pipe` does or once
/// the streams are over and `pipe` has nothing more at hand: kube's pipes
/// stay open when the stream breaks. kube writes each frame into its pipe
/// before it reads the next, so everything sent before the exit status is in
/// `pipe` by the time that status is.
fn forward(
    mut pipe: impl AsyncRead + Unpin + Send + 'static,
    settled: watch::Receiver<bool>,
) -> DuplexStream {
    let (reader, mut out) = tokio::io::duplex(PIPE);
    tokio::spawn(async move {
        let mut chunk = vec![0; PIPE];
        let mut over = Box::pin(over(settled));
        loop {
            let read = tokio::select! {
                biased;
                read = pipe.read(&mut chunk) => read,
                () = &mut over => break,
            };
            match read {
                Ok(read @ 1..) if out.write_all(&chunk[..read]).await.is_ok() => {}
                // The caller stopped reading. kube waits on a full pipe, and
                // would never pass the exit status on.
                Ok(1..) => {
                    let mut nowhere = tokio::io::sink();
                    tokio::select! {
                        _ = tokio::io::copy(&mut pipe, &mut nowhere) => {}
                        () = &mut over => {}
                    }
                    return;
                }
                _ => return,
            }
        }
        while let Some(Ok(read @ 1..)) =
            tokio::task::unconstrained(pipe.read(&mut chunk)).now_or_never()
        {
            if out.write_all(&chunk[..read]).await.is_err() {
                return;
            }
        }
    });
    reader
}

/// The last [`STDERR_TAIL`] bytes of `stderr`, read to its end.
async fn stderr_tail(mut stderr: DuplexStream) -> String {
    let mut kept = Vec::new();
    let mut chunk = vec![0; STDERR_CHUNK];
    while let Ok(read @ 1..) = stderr.read(&mut chunk).await {
        kept.extend_from_slice(&chunk[..read]);
        if kept.len() > STDERR_TAIL {
            kept.drain(..kept.len() - STDERR_TAIL);
        }
    }
    String::from_utf8_lossy(&kept).trim().to_owned()
}

/// A stdin the caller ends by shutting it down or dropping it, copied into
/// kube's, which only ends on a shutdown. The copy stops when the process
/// does, so a caller writing to a process that is gone gets an error instead
/// of waiting.
fn feed(mut sink: WriteHalf<SimplexStream>, settled: watch::Receiver<bool>) -> DuplexStream {
    let (caller, mut source) = tokio::io::duplex(PIPE);
    tokio::spawn(async move {
        let copy = async {
            if tokio::io::copy(&mut source, &mut sink).await.is_ok() {
                let _ = sink.shutdown().await;
            }
        };
        tokio::select! {
            () = copy => {}
            () = over(settled) => {}
        }
    });
    caller
}

/// How the exec ended. Owns `process`, whose drop would stop the streams, so
/// the exec lives as long as this future does.
async fn ended(
    process: AttachedProcess,
    status: JoinHandle<Option<ExitStatus>>,
    stderr: JoinHandle<String>,
) -> ExecEnd {
    let status = status.await.ok().flatten();
    let stderr = stderr.await.unwrap_or_default();
    let pumped = process.join().await;
    match status {
        Some(status) => finished(&status, &stderr),
        None => ExecEnd::Failed {
            message: match pumped {
                Err(err) => err.to_string(),
                Ok(()) => "the exec stream closed before an exit status arrived".to_owned(),
            },
        },
    }
}

/// What an exit status says, with the end of the process's stderr.
fn finished(status: &ExitStatus, stderr: &str) -> ExecEnd {
    if status.status.as_deref() == Some("Success") {
        return ExecEnd::Succeeded;
    }
    let reason = [&status.message, &status.reason]
        .into_iter()
        .flatten()
        .find(|text| !text.is_empty())
        .cloned()
        .unwrap_or_else(|| "the process failed, and its status says no more".to_owned());
    ExecEnd::Failed {
        message: if stderr.is_empty() {
            reason
        } else {
            format!("{reason}; stderr: {stderr}")
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_status_says_its_message_or_its_reason() {
        let status = |reason: &str, message: &str| Status::failure(message, reason).with_code(400);
        assert_eq!(
            describe(&status("Forbidden", "exceeded quota")),
            "exceeded quota"
        );
        assert_eq!(describe(&status("Invalid", "")), "Invalid");
        assert_eq!(describe(&status("", "")), "refused with HTTP 400");
    }

    /// An exec ends as its status says, with its stderr when there is any.
    #[test]
    fn an_exit_status_decides_the_end() {
        let exit = |status: &str, message: &str, reason: &str| ExitStatus {
            status: Some(status.to_owned()),
            message: Some(message.to_owned()),
            reason: Some(reason.to_owned()),
            ..ExitStatus::default()
        };
        let failed = |message: &str| ExecEnd::Failed {
            message: message.to_owned(),
        };
        let success = exit("Success", "", "");
        assert_eq!(finished(&success, "noise"), ExecEnd::Succeeded);
        let internal = exit("Failure", "", "InternalError");
        assert_eq!(finished(&internal, ""), failed("InternalError"));
        let silent = failed("the process failed, and its status says no more");
        assert_eq!(finished(&ExitStatus::default(), ""), silent);
        let nonzero = "command terminated with non-zero exit code: exit code 3";
        let code3 = exit("Failure", nonzero, "NonZeroExitCode");
        let with_stderr = failed(&format!("{nonzero}; stderr: oops"));
        assert_eq!(finished(&code3, "oops"), with_stderr);
        assert_eq!(finished(&code3, ""), failed(nonzero));
    }

    struct Piped {
        writer: WriteHalf<SimplexStream>,
        output: DuplexStream,
        /// Held so that the streams are not over.
        _settled: watch::Sender<bool>,
    }

    fn piped() -> Piped {
        let (reader, writer) = tokio::io::simplex(64);
        let (settled, is_settled) = watch::channel(false);
        Piped {
            writer,
            output: forward(reader, is_settled),
            _settled: settled,
        }
    }

    /// kube's pipe, which stays open when the stream breaks, with its last
    /// bytes not at hand on the first read.
    struct Open {
        last: &'static [u8],
        polled: bool,
    }

    impl AsyncRead for Open {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            if std::mem::replace(&mut self.polled, true) && !self.last.is_empty() {
                buf.put_slice(std::mem::take(&mut self.last));
                return std::task::Poll::Ready(Ok(()));
            }
            std::task::Poll::Pending
        }
    }

    /// What kube's pipe held when the streams ended is passed on, and the
    /// output then ends although the pipe stays open.
    #[tokio::test]
    async fn output_ends_after_its_last_byte_once_the_streams_are_over() {
        let (_settled, is_settled) = watch::channel(true);
        let open = Open {
            last: b"last",
            polled: false,
        };
        let mut output = forward(open, is_settled);
        let mut read = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), output.read_to_end(&mut read))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(read, b"last");
    }

    #[tokio::test]
    async fn output_waits_while_the_process_runs() {
        let mut pipe = piped();
        let mut byte = [0; 1];
        let early =
            tokio::time::timeout(Duration::from_millis(50), pipe.output.read(&mut byte)).await;
        assert!(early.is_err(), "a read returned while nothing was written");
        pipe.writer.write_all(b"x").await.unwrap();
        assert_eq!(pipe.output.read(&mut byte).await.unwrap(), 1);
    }

    /// Output that the caller dropped is still read, so kube reaches the
    /// exit status.
    #[tokio::test]
    async fn output_that_the_caller_dropped_is_drained() {
        let mut pipe = piped();
        drop(pipe.output);
        let more = vec![0; 4 * PIPE];
        let written = tokio::time::timeout(Duration::from_secs(5), pipe.writer.write_all(&more));
        assert!(written.await.is_ok(), "kube's pipe filled up");
    }

    #[tokio::test]
    async fn stderr_keeps_its_end() {
        let mut pipe = piped();
        let reading = tokio::spawn(stderr_tail(pipe.output));
        let head = vec![b'a'; 3 * STDERR_TAIL];
        pipe.writer.write_all(&head).await.unwrap();
        pipe.writer.write_all(b"the end\n").await.unwrap();
        pipe.writer.shutdown().await.unwrap();
        let kept = reading.await.unwrap();
        assert!(kept.ends_with("the end"));
        assert!(kept.len() <= STDERR_TAIL);
    }

    #[tokio::test]
    async fn fed_stdin_ends_when_dropped() {
        let (mut reader, writer) = tokio::io::simplex(64);
        let (_settled, is_settled) = watch::channel(false);
        let mut caller = feed(writer, is_settled);
        caller.write_all(b"job").await.unwrap();
        drop(caller);
        let mut read = Vec::new();
        reader.read_to_end(&mut read).await.unwrap();
        assert_eq!(read, b"job");
    }

    #[tokio::test]
    async fn fed_stdin_fails_once_the_process_is_gone() {
        let (_reader, writer) = tokio::io::simplex(64);
        let (settled, is_settled) = watch::channel(false);
        let mut caller = feed(writer, is_settled);
        settled.send_replace(true);
        let big = vec![0; 4 * PIPE];
        let written = tokio::time::timeout(Duration::from_secs(5), caller.write_all(&big)).await;
        assert!(
            matches!(written, Ok(Err(_))),
            "a write to a finished process did not fail"
        );
    }
}
