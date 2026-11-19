//! [`Cluster`] over a real API server, through kube-rs.
//!
//! One watch on the builds' namespace, filtered by this dispatcher's label,
//! feeds a reflector store, and through it [`Cluster::pod`],
//! [`Cluster::pods`] and [`Cluster::generation`]; everything else is one API
//! call.
//!
//! An exec's stdout is read only as fast as the caller reads it: kube moves
//! the WebSocket's frames into a pipe of [`PIPE`] bytes and stops reading the
//! socket while that pipe is full, so the process in the container is slowed
//! to the caller's pace and nothing is buffered whole. Its stderr goes through
//! a pipe of its own, drained by a task of its own, because a full stderr pipe
//! would stop stdout and the exit status with it.

use crate::cluster::{Admission, Cluster, CreateError, Exec, ExecEnd, PROMPT, Stdin};
use crate::pod;
use anyhow::{Context, Result, ensure};
use futures::StreamExt;
use futures::future::BoxFuture;
use k8s_openapi::api::core::v1::{Container, Pod};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Status as ExitStatus;
use kube::api::{
    Api, AttachParams, AttachedProcess, DeleteParams, ListParams, LogParams, PostParams,
};
use kube::core::Status;
use kube::runtime::WatchStreamExt;
use kube::runtime::reflector::store::Writer;
use kube::runtime::reflector::{ObjectRef, Store};
use kube::runtime::watcher::{self, Event};
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll, ready};
use std::time::Duration;
use tokio::io::{
    AsyncRead, AsyncReadExt, AsyncWriteExt, DuplexStream, ReadBuf, ReadHalf, SimplexStream,
    WriteHalf,
};
use tokio::sync::watch;
use tokio::task::JoinHandle;

/// The most an exec's stdin, stdout or stderr pipe holds: how far the caller
/// may fall behind the process before the process waits.
const PIPE: usize = 64 * 1024;
/// How much of an exec's stderr a failure message carries: its last bytes.
const STDERR_TAIL: usize = 4 * 1024;
/// How long to wait before watching again after the watch's stream ended.
const REWATCH: Duration = Duration::from_secs(5);

/// The builds' namespace through one client, and the watch on this
/// dispatcher's pods in it.
pub struct KubeCluster {
    client: kube::Client,
    pods: Api<Pod>,
    namespace: String,
    selector: String,
    store: Store<Pod>,
    generation: watch::Receiver<u64>,
    _watch: Watch,
}

/// The task that keeps the store current, stopped with the cluster.
struct Watch(JoinHandle<()>);

impl Drop for Watch {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl KubeCluster {
    /// Connect (in-cluster, or `KUBECONFIG`), start the watch on this
    /// dispatcher's pods, and return once it has listed them.
    ///
    /// kube's TLS takes rustls' process-wide crypto provider, so one must be
    /// installed before this is called.
    pub async fn connect(namespace: &str, dispatcher: &str) -> Result<Self> {
        ensure!(
            rustls::crypto::CryptoProvider::get_default().is_some(),
            "no rustls crypto provider is installed for the Kubernetes client"
        );
        let client = kube::Client::try_default()
            .await
            .context("connecting to the Kubernetes API")?;
        let pods: Api<Pod> = Api::namespaced(client.clone(), namespace);
        let selector = pod::selector(dispatcher);
        // The watch retries a refused list for ever; one plain list first
        // turns a missing permission into an error here instead.
        pods.list(&ListParams::default().labels(&selector).limit(1))
            .await
            .with_context(|| format!("listing pods in namespace {namespace}"))?;
        let writer = Writer::default();
        let store = writer.as_reader();
        let (moved, generation) = watch::channel(0);
        let task = tokio::spawn(follow(pods.clone(), selector.clone(), writer, moved));
        store
            .wait_until_ready()
            .await
            .context("the watch on pods stopped before it listed them")?;
        Ok(Self {
            client,
            pods,
            namespace: namespace.to_owned(),
            selector,
            store,
            generation,
            _watch: Watch(task),
        })
    }

    /// The image a pod runs, by digest where its status has a pullable one.
    pub async fn image_of(&self, namespace: &str, pod: &str) -> Result<String> {
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), namespace);
        let found = pods
            .get(pod)
            .await
            .with_context(|| format!("reading pod {namespace}/{pod}"))?;
        running_image(&found)
    }
}

impl Cluster for KubeCluster {
    async fn create(&self, pod: &Pod) -> Result<Pod, CreateError> {
        let err = match self.pods.create(&PostParams::default(), pod).await {
            Ok(created) => return Ok(created),
            Err(err) => err,
        };
        Err(match rejection(&err) {
            Rejection::Refused { message } => CreateError::Refused { message },
            Rejection::Exists => CreateError::Exists,
            Rejection::Other => {
                CreateError::Other(anyhow::Error::new(err).context("creating a pod"))
            }
        })
    }

    async fn admits(&self, pod: &Pod) -> Admission {
        let params = PostParams {
            dry_run: true,
            ..PostParams::default()
        };
        match self.pods.create(&params, pod).await {
            Ok(pod) => Admission::Admitted { pod: Box::new(pod) },
            Err(err) => admission(&err),
        }
    }

    async fn delete(&self, name: &str, grace: Duration) -> Result<()> {
        let params = DeleteParams {
            grace_period_seconds: Some(u32::try_from(grace.as_secs()).unwrap_or(u32::MAX)),
            ..DeleteParams::default()
        };
        match self.pods.delete(name, &params).await {
            Ok(_) => Ok(()),
            Err(kube::Error::Api(status)) if status.is_not_found() => Ok(()),
            Err(err) => Err(err).with_context(|| format!("deleting pod {name}")),
        }
    }

    async fn sweep(&self) -> Result<usize> {
        let params = DeleteParams {
            grace_period_seconds: Some(u32::try_from(PROMPT.as_secs()).unwrap_or(1)),
            ..DeleteParams::default()
        };
        let list = ListParams::default().labels(&self.selector);
        let deleted = self
            .pods
            .delete_collection(&params, &list)
            .await
            .with_context(|| format!("deleting the pods labelled {}", self.selector))?;
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
        let streams = Streams::take(&mut process, stdin)?;
        let (pump, over) = watch::channel(Pump::Running);
        let status = tokio::spawn(settle(streams.status, pump));
        let stderr = tokio::spawn(tail(Output::new(streams.stderr, over.clone())));
        let stdin = streams.stdin.map(|sink| {
            Box::pin(feed(sink, over.clone())) as Pin<Box<dyn tokio::io::AsyncWrite + Send>>
        });
        Ok(Exec {
            stdin,
            stdout: Box::pin(Output::new(streams.stdout, over)),
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
/// whenever that changed what the store holds. A watch whose stream ends is
/// started again, and lists afresh.
async fn follow(
    pods: Api<Pod>,
    selector: String,
    mut writer: Writer<Pod>,
    generation: watch::Sender<u64>,
) {
    let config = watcher::Config::default().labels(&selector);
    loop {
        let mut events = watcher::watcher(pods.clone(), config.clone())
            .default_backoff()
            .boxed();
        while let Some(event) = events.next().await {
            match event {
                Ok(event) => {
                    writer.apply_watcher_event(&event);
                    if changes_store(&event) {
                        generation.send_modify(|count| *count = count.wrapping_add(1));
                    }
                }
                Err(err) => tracing::warn!("watching pods labelled {selector}: {err}"),
            }
        }
        tracing::warn!("the watch on pods labelled {selector} ended; watching again");
        tokio::time::sleep(REWATCH).await;
    }
}

/// Whether applying `event` changed what the store holds. A relist only
/// replaces the store when it is complete.
fn changes_store(event: &Event<Pod>) -> bool {
    matches!(event, Event::Apply(_) | Event::Delete(_) | Event::InitDone)
}

/// How an API error answers a request to create a pod.
#[derive(Debug, PartialEq, Eq)]
enum Rejection {
    /// Admission or validation said no to this pod.
    Refused { message: String },
    /// A pod of that name exists.
    Exists,
    /// Anything else: the request did not get an answer about the pod.
    Other,
}

fn rejection(err: &kube::Error) -> Rejection {
    let kube::Error::Api(status) = err else {
        return Rejection::Other;
    };
    match status.code {
        409 => Rejection::Exists,
        400 | 403 | 422 => Rejection::Refused {
            message: describe(status),
        },
        _ => Rejection::Other,
    }
}

/// How an API error answers whether the cluster would admit a pod.
///
/// Whatever the API server itself answers with, other than a taken name,
/// is what a pod created now would get too: not only admission's refusals,
/// but a namespace that is not there (404), a webhook the server must ask
/// and cannot (5xx), and a server shedding load (429). A question that
/// reached no server says a create would reach none either.
fn admission(err: &kube::Error) -> Admission {
    let kube::Error::Api(status) = err else {
        return Admission::Unreached {
            message: err.to_string(),
        };
    };
    match status.code {
        409 => Admission::Unjudged {
            message: "a pod of that name exists, so the dry run could not judge it".to_owned(),
        },
        400..=599 => Admission::Refused {
            message: describe(status),
        },
        _ => Admission::Unreached {
            message: err.to_string(),
        },
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

/// The image of this worker's own container in `pod`, by the digest it was
/// pulled at when its status has one that can be pulled again.
fn running_image(pod: &Pod) -> Result<String> {
    let own = pod
        .spec
        .as_ref()
        .and_then(|spec| own_container(&spec.containers))
        .context("the pod has no containers")?;
    let pulled = pod
        .status
        .as_ref()
        .and_then(|status| status.container_statuses.as_ref())
        .and_then(|statuses| statuses.iter().find(|status| status.name == own.name))
        .and_then(|status| pullable(&status.image_id));
    match pulled {
        Some(image) => Ok(image),
        None => own
            .image
            .clone()
            .with_context(|| format!("container {} names no image", own.name)),
    }
}

/// This worker's container among a pod's: the one told which pod it is in,
/// which is how it came to ask, and the first where none is, so that a
/// sidecar listed before it is not taken for it.
fn own_container(containers: &[Container]) -> Option<&Container> {
    let told = |container: &&Container| {
        container
            .env
            .iter()
            .flatten()
            .any(|var| var.name == crate::config::POD_NAME)
    };
    containers.iter().find(told).or_else(|| containers.first())
}

/// `image_id` as a reference that pulls the same image: `<repository>@<digest>`.
/// `None` for an id that is only a digest or a local image.
fn pullable(image_id: &str) -> Option<String> {
    let id = image_id
        .strip_prefix("docker-pullable://")
        .unwrap_or(image_id);
    let (repository, digest) = id.split_once('@')?;
    let (algorithm, hex) = digest.split_once(':')?;
    let named = !repository.is_empty() && !repository.contains("://");
    let digested =
        !algorithm.is_empty() && !hex.is_empty() && hex.bytes().all(|b| b.is_ascii_hexdigit());
    (named && digested).then(|| id.to_owned())
}

/// The ends of an exec this module keeps or hands on.
struct Streams {
    stdin: Option<WriteHalf<SimplexStream>>,
    stdout: ReadHalf<SimplexStream>,
    stderr: ReadHalf<SimplexStream>,
    status: BoxFuture<'static, Option<ExitStatus>>,
}

impl Streams {
    fn take(process: &mut AttachedProcess, stdin: Stdin) -> Result<Self> {
        let stdin = match stdin {
            Stdin::Piped => Some(process.stdin().context("the exec has no stdin")?),
            Stdin::Closed => None,
        };
        Ok(Self {
            stdin,
            stdout: process.stdout().context("the exec has no stdout")?,
            stderr: process.stderr().context("the exec has no stderr")?,
            status: Box::pin(process.take_status().context("the exec has no status")?),
        })
    }
}

/// Where the task that moves an exec's streams is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pump {
    Running,
    /// The process's exit status arrived: every byte it wrote is in the pipes.
    Finished,
    /// The stream ended without an exit status.
    Broken,
}

/// Wait for the exit status, and say on `pump` that the streams are over.
async fn settle(
    status: BoxFuture<'static, Option<ExitStatus>>,
    pump: watch::Sender<Pump>,
) -> Option<ExitStatus> {
    let status = status.await;
    let over = if status.is_some() {
        Pump::Finished
    } else {
        Pump::Broken
    };
    pump.send_replace(over);
    status
}

/// Resolves once `pump` is no longer [`Pump::Running`].
async fn over(mut pump: watch::Receiver<Pump>) -> Pump {
    match pump.wait_for(|state| *state != Pump::Running).await {
        Ok(state) => *state,
        Err(_) => Pump::Broken,
    }
}

/// One of an exec's output pipes, ending where the process's output does.
///
/// kube's pipes do not say where that is. One stays open when the stream
/// breaks, and one is closed when the stream ends, before kube lets go of
/// the status, whether one came or not. So a read that finds nothing asks
/// the pump: this ends once the exit status is in, and fails for a stream
/// that ended without one. A reader never waits on a pipe nothing writes to,
/// and never takes a stream cut short for one read to its end.
struct Output {
    pipe: ReadHalf<SimplexStream>,
    pump: Settling,
}

enum Settling {
    Waiting(BoxFuture<'static, Pump>),
    Settled(Pump),
}

impl Output {
    fn new(pipe: ReadHalf<SimplexStream>, pump: watch::Receiver<Pump>) -> Self {
        Self {
            pipe,
            pump: Settling::Waiting(Box::pin(over(pump))),
        }
    }
}

impl AsyncRead for Output {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        if let Some(read) = gave(&mut this.pipe, cx, buf) {
            return Poll::Ready(read);
        }
        let state = match &mut this.pump {
            Settling::Settled(state) => *state,
            Settling::Waiting(over) => {
                let state = ready!(over.as_mut().poll(cx));
                this.pump = Settling::Settled(state);
                // What was written before the pump settled is in the pipe
                // now, even if the read above came too early to see it.
                if let Some(read) = gave(&mut this.pipe, cx, buf) {
                    return Poll::Ready(read);
                }
                state
            }
        };
        Poll::Ready(match state {
            Pump::Broken => Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the exec stream broke before the process ended",
            )),
            Pump::Running | Pump::Finished => Ok(()),
        })
    }
}

/// What a read of `pipe` into `buf` gave: bytes, or an error. `None` when it
/// gave nothing, the pipe being empty for now or at its end, which only the
/// pump tells apart from the end of the process's output.
fn gave(
    pipe: &mut ReadHalf<SimplexStream>,
    cx: &mut TaskContext<'_>,
    buf: &mut ReadBuf<'_>,
) -> Option<io::Result<()>> {
    let before = buf.filled().len();
    match Pin::new(pipe).poll_read(cx, buf) {
        Poll::Ready(Ok(())) if buf.filled().len() == before => None,
        Poll::Ready(read) => Some(read),
        Poll::Pending => None,
    }
}

/// The last [`STDERR_TAIL`] bytes of `stderr`, read to its end.
async fn tail(mut stderr: Output) -> String {
    let mut kept = Vec::new();
    let mut chunk = vec![0; 8 * 1024];
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
fn feed(mut sink: WriteHalf<SimplexStream>, pump: watch::Receiver<Pump>) -> DuplexStream {
    let (caller, mut source) = tokio::io::duplex(PIPE);
    tokio::spawn(async move {
        let copy = async {
            if tokio::io::copy(&mut source, &mut sink).await.is_ok() {
                let _ = sink.shutdown().await;
            }
        };
        tokio::select! {
            () = copy => {}
            _ = over(pump) => {}
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
        None => ExecEnd::Lost {
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
    use k8s_openapi::api::core::v1::{ContainerStatus, PodSpec, PodStatus};

    fn api(code: u16, reason: &str, message: &str) -> kube::Error {
        kube::Error::Api(Box::new(Status::failure(message, reason).with_code(code)))
    }

    #[test]
    fn admission_and_validation_refuse() {
        let quota = "pods \"x\" is forbidden: exceeded quota: builds, requested: pods=1";
        assert_eq!(
            rejection(&api(403, "Forbidden", quota)),
            Rejection::Refused {
                message: quota.to_owned()
            }
        );
        assert_eq!(
            rejection(&api(422, "Invalid", "")),
            Rejection::Refused {
                message: "Invalid".to_owned()
            }
        );
        assert_eq!(
            rejection(&api(400, "", "")),
            Rejection::Refused {
                message: "refused with HTTP 400".to_owned()
            }
        );
    }

    #[test]
    fn a_taken_name_exists() {
        assert_eq!(
            rejection(&api(409, "AlreadyExists", "exists")),
            Rejection::Exists
        );
    }

    /// What the API server answers a dry run with is what a pod would get.
    #[test]
    fn a_dry_run_the_server_answers_no_to_is_a_refusal() {
        for (code, reason, message) in [
            (403, "Forbidden", "exceeded quota: builds"),
            (404, "NotFound", "namespaces \"aurcache-builds\" not found"),
            (422, "Invalid", "spec.containers: Required value"),
            (429, "TooManyRequests", "slow down"),
            (500, "InternalError", "failed calling webhook"),
            (503, "ServiceUnavailable", "the server is currently unable"),
        ] {
            assert_eq!(
                admission(&api(code, reason, message)),
                Admission::Refused {
                    message: message.to_owned()
                },
                "for {code}"
            );
        }
    }

    #[test]
    fn a_taken_name_is_not_judged_and_no_server_is_not_reached() {
        assert!(matches!(
            admission(&api(409, "AlreadyExists", "exists")),
            Admission::Unjudged { .. }
        ));
        assert!(matches!(
            admission(&kube::Error::LinesCodecMaxLineLengthExceeded),
            Admission::Unreached { .. }
        ));
    }

    #[test]
    fn other_failures_are_no_answer() {
        assert_eq!(
            rejection(&api(500, "InternalError", "webhook down")),
            Rejection::Other
        );
        assert_eq!(
            rejection(&api(429, "TooManyRequests", "slow down")),
            Rejection::Other
        );
        assert_eq!(
            rejection(&api(404, "NotFound", "no namespace")),
            Rejection::Other
        );
        assert_eq!(
            rejection(&kube::Error::LinesCodecMaxLineLengthExceeded),
            Rejection::Other
        );
    }

    fn exit(status: &str, message: Option<&str>, reason: Option<&str>) -> ExitStatus {
        ExitStatus {
            status: Some(status.to_owned()),
            message: message.map(str::to_owned),
            reason: reason.map(str::to_owned),
            ..ExitStatus::default()
        }
    }

    #[test]
    fn a_success_status_succeeds() {
        assert_eq!(
            finished(&exit("Success", None, None), "noise"),
            ExecEnd::Succeeded
        );
    }

    #[test]
    fn a_failure_without_a_message_gives_its_reason() {
        assert_eq!(
            finished(&exit("Failure", Some(""), Some("InternalError")), ""),
            ExecEnd::Failed {
                message: "InternalError".to_owned()
            }
        );
        assert!(matches!(
            finished(&ExitStatus::default(), ""),
            ExecEnd::Failed { .. }
        ));
    }

    #[test]
    fn a_failure_status_carries_stderr() {
        let status = exit(
            "Failure",
            Some("command terminated with non-zero exit code: exit code 3"),
            Some("NonZeroExitCode"),
        );
        assert_eq!(
            finished(&status, "oops"),
            ExecEnd::Failed {
                message: "command terminated with non-zero exit code: exit code 3; stderr: oops"
                    .to_owned()
            }
        );
        assert_eq!(
            finished(&status, ""),
            ExecEnd::Failed {
                message: "command terminated with non-zero exit code: exit code 3".to_owned()
            }
        );
    }

    const DIGEST: &str = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn with_images(image: &str, statuses: &[(&str, &str)]) -> Pod {
        Pod {
            spec: Some(PodSpec {
                containers: vec![
                    Container {
                        name: "first".to_owned(),
                        image: Some(image.to_owned()),
                        ..Container::default()
                    },
                    Container {
                        name: "second".to_owned(),
                        image: Some("other:1".to_owned()),
                        ..Container::default()
                    },
                ],
                ..PodSpec::default()
            }),
            status: Some(PodStatus {
                container_statuses: Some(
                    statuses
                        .iter()
                        .map(|(name, id)| ContainerStatus {
                            name: (*name).to_owned(),
                            image_id: (*id).to_owned(),
                            ..ContainerStatus::default()
                        })
                        .collect(),
                ),
                ..PodStatus::default()
            }),
            ..Pod::default()
        }
    }

    #[test]
    fn a_registry_digest_is_the_image() {
        let id = format!("ghcr.io/x/worker@{DIGEST}");
        let pod = with_images("ghcr.io/x/worker:latest", &[("first", &id)]);
        assert_eq!(running_image(&pod).unwrap(), id);
    }

    #[test]
    fn dockers_pullable_prefix_is_stripped() {
        let pod = with_images(
            "busybox",
            &[("first", &format!("docker-pullable://busybox@{DIGEST}"))],
        );
        assert_eq!(running_image(&pod).unwrap(), format!("busybox@{DIGEST}"));
    }

    #[test]
    fn a_bare_or_local_id_falls_back_to_the_spec() {
        for id in [
            DIGEST.to_owned(),
            format!("docker://{DIGEST}"),
            String::new(),
            "busybox@latest".to_owned(),
        ] {
            let pod = with_images("local/worker:dev", &[("first", &id)]);
            assert_eq!(
                running_image(&pod).unwrap(),
                "local/worker:dev",
                "for {id:?}"
            );
        }
    }

    #[test]
    fn the_first_containers_status_is_found_by_name() {
        let id = format!("docker.io/library/busybox@{DIGEST}");
        let pod = with_images(
            "busybox",
            &[
                ("second", "docker.io/library/other@sha256:ff"),
                ("first", &id),
            ],
        );
        assert_eq!(running_image(&pod).unwrap(), id);
    }

    /// A sidecar listed first is not this worker.
    #[test]
    fn the_container_told_its_pods_name_is_this_workers() {
        let id = format!("ghcr.io/x/worker@{DIGEST}");
        let mut pod = with_images("proxy:1", &[("first", "proxy@sha256:ff"), ("second", &id)]);
        let containers = &mut pod.spec.as_mut().unwrap().containers;
        containers[1].env = Some(vec![k8s_openapi::api::core::v1::EnvVar {
            name: "WORKER_KUBE_POD_NAME".to_owned(),
            ..Default::default()
        }]);
        assert_eq!(running_image(&pod).unwrap(), id);
    }

    #[test]
    fn a_pod_without_status_runs_its_spec() {
        let mut pod = with_images("busybox:1.37", &[]);
        pod.status = None;
        assert_eq!(running_image(&pod).unwrap(), "busybox:1.37");
        assert!(running_image(&Pod::default()).is_err());
    }

    #[test]
    fn only_store_changes_move_the_generation() {
        assert!(changes_store(&Event::Apply(Pod::default())));
        assert!(changes_store(&Event::Delete(Pod::default())));
        assert!(changes_store(&Event::InitDone));
        assert!(!changes_store(&Event::Init));
        assert!(!changes_store(&Event::InitApply(Pod::default())));
    }

    struct Piped {
        writer: WriteHalf<SimplexStream>,
        output: Output,
        pump: watch::Sender<Pump>,
    }

    fn piped() -> Piped {
        let (reader, writer) = tokio::io::simplex(64);
        let (pump, over) = watch::channel(Pump::Running);
        Piped {
            writer,
            output: Output::new(reader, over),
            pump,
        }
    }

    #[tokio::test]
    async fn output_ends_after_its_last_byte_once_the_status_is_in() {
        let mut pipe = piped();
        pipe.writer.write_all(b"last").await.unwrap();
        pipe.pump.send_replace(Pump::Finished);
        let mut read = Vec::new();
        pipe.output.read_to_end(&mut read).await.unwrap();
        assert_eq!(read, b"last");
    }

    #[tokio::test]
    async fn output_fails_once_the_stream_broke() {
        let mut pipe = piped();
        pipe.writer.write_all(b"part").await.unwrap();
        pipe.pump.send_replace(Pump::Broken);
        let mut read = Vec::new();
        let err = pipe.output.read_to_end(&mut read).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
        assert_eq!(read, b"part");
    }

    /// The order kube ends a stream that closed without a status in: the
    /// pipe is shut down first, and the status let go of after. The pipe's
    /// end is not taken for the process's: the read waits for the pump, and
    /// fails.
    #[tokio::test]
    async fn output_shut_down_before_a_lost_status_fails() {
        let Piped {
            mut writer,
            mut output,
            pump,
        } = piped();
        writer.write_all(b"part").await.unwrap();
        writer.shutdown().await.unwrap();
        let reading = tokio::spawn(async move {
            let mut read = Vec::new();
            let err = output.read_to_end(&mut read).await.unwrap_err();
            assert_eq!(read, b"part");
            err.kind()
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !reading.is_finished(),
            "the read ended at the pipe's end, before the status was known"
        );
        pump.send_replace(Pump::Broken);
        assert_eq!(reading.await.unwrap(), io::ErrorKind::UnexpectedEof);
    }

    /// An empty buffer is no read at all, not the end.
    #[tokio::test]
    async fn an_empty_read_is_not_the_end() {
        let mut pipe = piped();
        pipe.writer.write_all(b"x").await.unwrap();
        assert_eq!(pipe.output.read(&mut []).await.unwrap(), 0);
        let mut byte = [0; 1];
        assert_eq!(pipe.output.read(&mut byte).await.unwrap(), 1);
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

    #[tokio::test]
    async fn stderr_keeps_its_end() {
        let mut pipe = piped();
        let reading = tokio::spawn(tail(pipe.output));
        let head = vec![b'a'; 3 * STDERR_TAIL];
        pipe.writer.write_all(&head).await.unwrap();
        pipe.writer.write_all(b"the end\n").await.unwrap();
        pipe.pump.send_replace(Pump::Finished);
        let kept = reading.await.unwrap();
        assert!(kept.ends_with("the end"));
        assert!(kept.len() <= STDERR_TAIL);
    }

    #[tokio::test]
    async fn fed_stdin_ends_when_dropped() {
        let (mut reader, writer) = tokio::io::simplex(64);
        let (_pump, over) = watch::channel(Pump::Running);
        let mut caller = feed(writer, over);
        caller.write_all(b"job").await.unwrap();
        drop(caller);
        let mut read = Vec::new();
        reader.read_to_end(&mut read).await.unwrap();
        assert_eq!(read, b"job");
    }

    #[tokio::test]
    async fn fed_stdin_fails_once_the_process_is_gone() {
        let (_reader, writer) = tokio::io::simplex(64);
        let (pump, over) = watch::channel(Pump::Running);
        let mut caller = feed(writer, over);
        pump.send_replace(Pump::Finished);
        let big = vec![0; 4 * PIPE];
        let written = tokio::time::timeout(Duration::from_secs(5), caller.write_all(&big)).await;
        assert!(
            matches!(written, Ok(Err(_))),
            "a write to a finished process did not fail"
        );
    }
}

/// Against a real API server. Ignored by default, and skipped unless
/// `AURCACHE_KUBE_TESTS=1`; they talk to whatever `KUBECONFIG` points at, in
/// `AURCACHE_KUBE_TEST_NAMESPACE` (default `aurcache-builds`), and pull
/// `busybox:1.37`.
///
/// The namespace is the one `kubernetes/kube-worker/` sets up, with the
/// credentials of its `aurcache-kube-worker` ServiceAccount, so they show that
/// the Role there is enough. One test needs a ResourceQuota that admits a
/// single pod with a deadline, put there by an administrator:
///
/// ```text
/// kubectl create namespace aurcache
/// kubectl apply -f kubernetes/kube-worker/namespace.yaml -f kubernetes/kube-worker/rbac.yaml
/// kubectl -n aurcache-builds create quota live-deadline --hard=pods=1 --scopes=Terminating
/// kubectl -n aurcache create token aurcache-kube-worker   # into a kubeconfig
/// AURCACHE_KUBE_TESTS=1 KUBECONFIG=... cargo test -p aurcache-worker-kube --lib \
///     kube_cluster::live -- --ignored --test-threads=1 --nocapture
/// ```
///
/// One thread, because the back-pressure test measures the process's memory.
#[cfg(test)]
mod live {
    use super::*;
    use crate::cluster::{Admission, CreateError, ExecEnd, Stdin};
    use std::time::{Instant, SystemTime};

    const IMAGE: &str = "busybox:1.37";
    const RUNNING: Duration = Duration::from_secs(180);

    fn enabled() -> bool {
        let on = std::env::var("AURCACHE_KUBE_TESTS").is_ok_and(|v| v == "1");
        if !on {
            eprintln!("skipped: set AURCACHE_KUBE_TESTS=1 and KUBECONFIG to run against a cluster");
        }
        on
    }

    fn namespace() -> String {
        std::env::var("AURCACHE_KUBE_TEST_NAMESPACE")
            .unwrap_or_else(|_| "aurcache-builds".to_owned())
    }

    /// A name no earlier run used: `live-<tag>-<clock>`.
    fn unique(tag: &str) -> String {
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos();
        format!("live-{tag}-{nanos:08x}")
    }

    async fn connect(dispatcher: &str) -> KubeCluster {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        KubeCluster::connect(&namespace(), dispatcher)
            .await
            .unwrap()
    }

    /// A busybox pod of `dispatcher` running `script`, never restarted.
    fn busybox(dispatcher: &str, name: &str, script: &str) -> Pod {
        serde_json::from_value(serde_json::json!({
            "metadata": { "name": name, "labels": pod::labels(dispatcher) },
            "spec": {
                "restartPolicy": "Never",
                "terminationGracePeriodSeconds": 0,
                "containers": [{
                    "name": "main",
                    "image": IMAGE,
                    "imagePullPolicy": "IfNotPresent",
                    "command": ["sh", "-c", script],
                }],
            },
        }))
        .unwrap()
    }

    fn phase(pod: &Pod) -> &str {
        pod.status
            .as_ref()
            .and_then(|status| status.phase.as_deref())
            .unwrap_or("")
    }

    /// Wait until the watch shows `name` as `wanted` says, or fail after `limit`.
    async fn until(
        cluster: &KubeCluster,
        name: &str,
        limit: Duration,
        wanted: impl Fn(Option<&Pod>) -> bool,
    ) {
        let mut moved = cluster.generation();
        let deadline = tokio::time::Instant::now() + limit;
        while !wanted(cluster.pod(name).as_deref()) {
            let waited = tokio::time::timeout_at(deadline, moved.changed()).await;
            assert!(waited.is_ok(), "the watch never showed {name} as wanted");
        }
    }

    async fn running(cluster: &KubeCluster, dispatcher: &str, name: &str) {
        cluster
            .create(&busybox(dispatcher, name, "sleep 3600"))
            .await
            .unwrap();
        until(cluster, name, RUNNING, |pod| {
            pod.is_some_and(|pod| phase(pod) == "Running")
        })
        .await;
    }

    struct Ran {
        stdout: Vec<u8>,
        end: ExecEnd,
    }

    async fn run(cluster: &KubeCluster, pod: &str, argv: &[&str]) -> Ran {
        let mut exec = cluster
            .exec(pod, "main", argv, Stdin::Closed)
            .await
            .unwrap();
        let mut stdout = Vec::new();
        let _ = exec.stdout.read_to_end(&mut stdout).await;
        Ran {
            stdout,
            end: exec.ended.await,
        }
    }

    fn resident() -> u64 {
        let status = std::fs::read_to_string("/proc/self/status").unwrap();
        let line = status
            .lines()
            .find(|line| line.starts_with("VmRSS:"))
            .unwrap();
        let kib: u64 = line.split_whitespace().nth(1).unwrap().parse().unwrap();
        kib * 1024
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a cluster: AURCACHE_KUBE_TESTS=1"]
    async fn a_pod_is_watched_to_its_end() {
        if !enabled() {
            return;
        }
        let dispatcher = unique("watch");
        let cluster = connect(&dispatcher).await;
        assert!(cluster.pods().is_empty());
        let name = unique("done");
        let pod = busybox(&dispatcher, &name, "echo first; echo second");
        cluster.create(&pod).await.unwrap();
        assert!(matches!(
            cluster.create(&pod).await,
            Err(CreateError::Exists)
        ));
        until(&cluster, &name, RUNNING, |pod| {
            pod.is_some_and(|pod| phase(pod) == "Succeeded")
        })
        .await;
        assert_eq!(cluster.pods().len(), 1);
        assert_eq!(
            cluster.log_tail(&name, "main", 1).await.unwrap(),
            "second\n"
        );
        cluster.delete(&name, PROMPT).await.unwrap();
        until(&cluster, &name, RUNNING, |pod| pod.is_none()).await;
        cluster.delete(&name, PROMPT).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a cluster: AURCACHE_KUBE_TESTS=1"]
    async fn exec_streams_and_reports() {
        if !enabled() {
            return;
        }
        let dispatcher = unique("exec");
        let cluster = connect(&dispatcher).await;
        let name = unique("sleep");
        running(&cluster, &dispatcher, &name).await;
        let image = cluster.image_of(&namespace(), &name).await.unwrap();
        assert!(image.contains("busybox@sha256:"), "{image}");

        // stdin in, stdout back, and a dropped stdin is its end.
        let sent: Vec<u8> = (0..4 * 1024 * 1024u32).map(|i| (i % 251) as u8).collect();
        let mut exec = cluster
            .exec(&name, "main", &["cat"], Stdin::Piped)
            .await
            .unwrap();
        let mut stdin = exec.stdin.take().unwrap();
        let input = sent.clone();
        let writing = tokio::spawn(async move {
            stdin.write_all(&input).await.unwrap();
        });
        let mut echoed = Vec::new();
        exec.stdout.read_to_end(&mut echoed).await.unwrap();
        writing.await.unwrap();
        assert_eq!(echoed.len(), sent.len());
        assert!(echoed == sent);
        assert_eq!(exec.ended.await, ExecEnd::Succeeded);

        let failed = run(&cluster, &name, &["false"]).await;
        let ExecEnd::Failed { message } = failed.end else {
            panic!("false ended {:?}", failed.end);
        };
        assert!(message.contains("exit code 1"), "{message}");

        let loud = run(
            &cluster,
            &name,
            &["sh", "-c", "echo out; echo oops >&2; exit 3"],
        )
        .await;
        assert_eq!(loud.stdout, b"out\n");
        let ExecEnd::Failed { message } = loud.end else {
            panic!("exit 3 ended {:?}", loud.end);
        };
        assert!(
            message.contains("exit code 3") && message.ends_with("stderr: oops"),
            "{message}"
        );

        // Far more stderr than its pipe holds, written before stdout's end.
        let noisy = run(
            &cluster,
            &name,
            &[
                "sh",
                "-c",
                "head -c 20000000 /dev/zero | tr '\\0' e >&2; echo done",
            ],
        )
        .await;
        assert_eq!(noisy.stdout, b"done\n");
        assert_eq!(noisy.end, ExecEnd::Succeeded);

        cluster.delete(&name, PROMPT).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a cluster: AURCACHE_KUBE_TESTS=1"]
    async fn a_slow_reader_holds_the_process_back() {
        if !enabled() {
            return;
        }
        const TOTAL: u64 = 300 * 1024 * 1024;
        const STALL: Duration = Duration::from_secs(10);
        // The reader's pace: 64 KiB every 2 ms, at most about 31 MiB/s.
        const CHUNK: usize = 64 * 1024;
        const PACE: Duration = Duration::from_millis(2);
        let dispatcher = unique("slow");
        let cluster = connect(&dispatcher).await;
        let name = unique("zero");
        running(&cluster, &dispatcher, &name).await;

        let before = resident();
        let command = format!("head -c {TOTAL} /dev/zero");
        let mut exec = cluster
            .exec(&name, "main", &["sh", "-c", &command], Stdin::Closed)
            .await
            .unwrap();
        // Nothing read: the process, which could write all of it in well
        // under a second, must still be waiting at the end of this.
        let stalled = tokio::time::timeout(STALL, &mut exec.ended).await;
        assert!(
            stalled.is_err(),
            "the process ended while nobody read its output"
        );
        let stalled_rss = resident();

        let started = Instant::now();
        let mut chunk = vec![0; CHUNK];
        let mut read = 0u64;
        let mut peak = stalled_rss;
        let mut reads = 0u64;
        loop {
            // A stream that broke is told by the exec's end, below.
            let got = match exec.stdout.read(&mut chunk).await {
                Ok(0) => break,
                Ok(got) => got,
                Err(error) => {
                    eprintln!("stdout ended with {error}");
                    break;
                }
            };
            assert!(chunk[..got].iter().all(|b| *b == 0));
            read += got as u64;
            reads += 1;
            if reads.is_multiple_of(64) {
                peak = peak.max(resident());
            }
            tokio::time::sleep(PACE).await;
        }
        let elapsed = started.elapsed();
        peak = peak.max(resident());
        let end = exec.ended.await;
        let mib = |bytes: u64| bytes as f64 / (1024.0 * 1024.0);
        eprintln!(
            "back-pressure: {:.0} MiB in {reads} reads over {:.1} s ({:.1} MiB/s); \
             resident {:.1} MiB before, {:.1} MiB after a {} s stall, {:.1} MiB at peak (+{:.1} MiB)",
            mib(read),
            elapsed.as_secs_f64(),
            mib(read) / elapsed.as_secs_f64(),
            mib(before),
            mib(stalled_rss),
            STALL.as_secs(),
            mib(peak),
            mib(peak.saturating_sub(before)),
        );
        // Together: a short count with `Lost` is a broken stream, one with
        // `Succeeded` a stream cut short that still said it succeeded.
        assert_eq!((read, &end), (TOTAL, &ExecEnd::Succeeded));
        assert!(
            peak.saturating_sub(before) < 32 * 1024 * 1024,
            "memory grew by {:.1} MiB",
            mib(peak.saturating_sub(before))
        );
        cluster.delete(&name, PROMPT).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a cluster: AURCACHE_KUBE_TESTS=1"]
    async fn an_exec_ends_with_its_pod() {
        if !enabled() {
            return;
        }
        let dispatcher = unique("gone");
        let cluster = connect(&dispatcher).await;
        let name = unique("doomed");
        running(&cluster, &dispatcher, &name).await;
        let mut exec = cluster
            .exec(
                &name,
                "main",
                &["sh", "-c", "echo started; exec sleep 3600"],
                Stdin::Closed,
            )
            .await
            .unwrap();
        let mut line = [0; 8];
        exec.stdout.read_exact(&mut line).await.unwrap();
        assert_eq!(&line, b"started\n");
        cluster.delete(&name, Duration::from_secs(1)).await.unwrap();
        let mut rest = Vec::new();
        let read = tokio::time::timeout(RUNNING, exec.stdout.read_to_end(&mut rest)).await;
        assert!(read.is_ok(), "stdout outlived its pod");
        let end = tokio::time::timeout(RUNNING, exec.ended).await.unwrap();
        eprintln!("an exec in a deleted pod: stdout {read:?}, ended {end:?}");
        assert_ne!(end, ExecEnd::Succeeded);
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a cluster: AURCACHE_KUBE_TESTS=1"]
    async fn sweep_deletes_this_dispatchers_pods_only() {
        if !enabled() {
            return;
        }
        let ours = unique("ours");
        let theirs = unique("theirs");
        let cluster = connect(&ours).await;
        let other = connect(&theirs).await;
        for _ in 0..3 {
            cluster
                .create(&busybox(&ours, &unique("swept"), "sleep 3600"))
                .await
                .unwrap();
        }
        let kept = unique("kept");
        cluster
            .create(&busybox(&theirs, &kept, "sleep 3600"))
            .await
            .unwrap();
        until(&other, &kept, RUNNING, |pod| pod.is_some()).await;

        assert_eq!(cluster.sweep().await.unwrap(), 3);
        let mut moved = cluster.generation();
        while !cluster.pods().is_empty() {
            tokio::time::timeout(RUNNING, moved.changed())
                .await
                .unwrap()
                .unwrap();
        }
        assert!(other.pod(&kept).is_some());
        assert_eq!(cluster.sweep().await.unwrap(), 0);
        assert_eq!(other.sweep().await.unwrap(), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a cluster: AURCACHE_KUBE_TESTS=1"]
    async fn a_dry_run_refuses_what_pod_security_forbids() {
        if !enabled() {
            return;
        }
        let dispatcher = unique("psa");
        let cluster = connect(&dispatcher).await;
        let name = unique("priv");
        let mut privileged = busybox(&dispatcher, &name, "true");
        let container = &mut privileged.spec.as_mut().unwrap().containers[0];
        container.security_context = Some(k8s_openapi::api::core::v1::SecurityContext {
            privileged: Some(true),
            ..Default::default()
        });
        let Admission::Refused { message } = cluster.admits(&privileged).await else {
            panic!("a privileged pod was admitted");
        };
        assert!(message.contains("privileged"), "{message}");
        assert!(matches!(
            cluster.create(&privileged).await,
            Err(CreateError::Refused { .. })
        ));

        let plain = busybox(&dispatcher, &name, "true");
        assert!(matches!(
            cluster.admits(&plain).await,
            Admission::Admitted { .. }
        ));
        // The dry run created nothing: the name is still free.
        cluster.create(&plain).await.unwrap();
        cluster.delete(&name, PROMPT).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a cluster: AURCACHE_KUBE_TESTS=1"]
    async fn a_dry_run_refuses_past_a_full_quota() {
        if !enabled() {
            return;
        }
        let dispatcher = unique("quota");
        let cluster = connect(&dispatcher).await;
        // The quota counts pods with a deadline, as every build pod has one.
        let deadline = |name: &str| {
            let mut pod = busybox(&dispatcher, name, "sleep 3600");
            pod.spec.as_mut().unwrap().active_deadline_seconds = Some(600);
            pod
        };
        let first = unique("first");
        let second = unique("second");
        // Checked, not consumed: two dry runs into room for one.
        assert!(matches!(
            cluster.admits(&deadline(&first)).await,
            Admission::Admitted { .. }
        ));
        assert!(matches!(
            cluster.admits(&deadline(&second)).await,
            Admission::Admitted { .. }
        ));
        cluster.create(&deadline(&first)).await.unwrap();
        let answer = cluster.admits(&deadline(&second)).await;
        cluster.delete(&first, PROMPT).await.unwrap();
        let Admission::Refused { message } = answer else {
            panic!("admitted past the quota ({answer:?}); is the quota from the setup there?");
        };
        assert!(message.contains("exceeded quota"), "{message}");
    }
}
