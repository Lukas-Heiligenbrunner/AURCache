//! The exchanges with a build pod, each one `exec` of the agent: handing the
//! job in (`receive`), following the log (`tail`), and handing the packages
//! back (`manifest`, then one `send` per package relayed straight into its
//! upload).
//!
//! Nothing here holds a package: no temporary file, no buffer of a whole file.
//! A package's bytes go from the exec's bounded pipe into the upload's request
//! body as the upload asks for them, and are counted and hashed on the way.

use crate::cluster::{Cluster, Exec, ExecEnd, Stdin};
use crate::outcome::{Observed, observe};
use crate::pod::{self, BUILD, HOLD, STAGE, WORK};
use anyhow::{Context, Result, anyhow, bail, ensure};
use aurcache_build_agent::report::{Artifact, Handback};
use aurcache_build_agent::spec::INTERFACE;
use aurcache_common::artifact::is_artifact;
use aurcache_worker_container::ssh::link::{self, Served};
use aurcache_worker_core::client::WorkerClient;
use aurcache_worker_core::protocol::PackagesRefused;
use sha2::{Digest, Sha256};
use std::io;
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context as TaskContext, Poll, ready};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt, ReadBuf};
use tokio::sync::mpsc;

/// The most a manifest may be. It is a line per package and a report of a
/// few lines; anything near this is not one.
const MANIFEST_LIMIT: u64 = 4 * 1024 * 1024;
/// The most log held back while waiting for the end of a line.
const LINE_CAP: usize = 64 * 1024;
/// How long to wait before following the log again after its stream broke.
const RESUME_DELAY: Duration = Duration::from_secs(2);
/// How long a `send` whose bytes were all taken may take to end.
const SEND_END: Duration = Duration::from_secs(60);
/// How much of a package is read at once when nothing else sizes the read.
const CHUNK: usize = 64 * 1024;

/// Where a build's packages go.
///
/// The server, through the worker's own client; a trait so the relay can be
/// tried against something that records what it was sent.
pub trait Uploads: Send + Sync {
    /// Send `body` as the package `name` of `build_id`, declaring `len`
    /// bytes. An error that is a [`PackagesRefused`] says the server will not
    /// take this file, whoever sends it.
    fn upload<R>(
        &self,
        build_id: i32,
        name: &str,
        body: R,
        len: u64,
    ) -> impl Future<Output = Result<()>> + Send
    where
        R: AsyncRead + Send + Unpin + 'static;
}

impl Uploads for WorkerClient {
    async fn upload<R>(&self, build_id: i32, name: &str, body: R, len: u64) -> Result<()>
    where
        R: AsyncRead + Send + Unpin + 'static,
    {
        self.upload_artifact(build_id, name, body, Some(len)).await
    }
}

/// The agent in one build pod, reached through `exec`.
pub struct Agent<'a, C: Cluster> {
    pub cluster: &'a C,
    /// The pod's name.
    pub pod: &'a str,
    /// Where the agent is in the `stage` and `hold` containers, which run the
    /// same image.
    pub bin: &'a str,
}

impl<C: Cluster> Agent<'_, C> {
    /// Hand the job in: `input` is the tar `receive` reads.
    ///
    /// Not to be tried twice: `receive` refuses a job that was already handed
    /// in, and a build may have started on the first.
    pub async fn receive(&self, input: &[u8]) -> Result<()> {
        let argv = [self.bin, "receive", "--work", WORK];
        let Exec {
            stdin,
            mut stdout,
            ended,
        } = self
            .cluster
            .exec(self.pod, STAGE, &argv, Stdin::Piped)
            .await?;
        let mut stdin = stdin.context("the exec gave no stdin to write the job to")?;
        let write = async {
            stdin.write_all(input).await?;
            stdin.shutdown().await
        };
        // Read while writing: a process that says something before it has
        // read everything must not be left waiting on a full pipe.
        let mut nowhere = tokio::io::sink();
        let drain = tokio::io::copy(&mut stdout, &mut nowhere);
        let (written, _) = tokio::join!(write, drain);
        match ended.await {
            // `receive` exits once it has read the archive's end, which may
            // be before this end of its stdin was closed: only its own word
            // says whether the job arrived.
            ExecEnd::Succeeded => Ok(()),
            ExecEnd::Failed { message } | ExecEnd::Lost { message } => match written {
                Ok(()) => bail!("{message}"),
                Err(error) => bail!("{message} (writing the job: {error})"),
            },
        }
    }

    /// Open the SSH link and pass what comes over it to the ssh-agent at
    /// `ssh_agent` until it ends: exec `ssh-link` into the build container,
    /// which connects to `socket`.
    ///
    /// Called once per build: the build agent takes one link, and has no use
    /// for a second.
    pub async fn ssh_link(&self, ssh_agent: &Path, socket: &Path) -> Served {
        let bin = pod::build_agent_path();
        let socket = socket.to_string_lossy();
        let argv = [bin.as_str(), "ssh-link", "--socket", &socket];
        let exec = match self
            .cluster
            .exec(self.pod, BUILD, &argv, Stdin::Piped)
            .await
        {
            Ok(exec) => exec,
            Err(error) => {
                return Served::lost(format!("the exec of ssh-link failed: {error:#}"));
            }
        };
        let Exec {
            stdin,
            stdout,
            ended,
        } = exec;
        let Some(stdin) = stdin else {
            return Served::lost("the exec gave no stdin".to_string());
        };
        let served = link::serve(ssh_agent, stdout, stdin).await;
        if let ExecEnd::Failed { message } | ExecEnd::Lost { message } = ended.await {
            tracing::debug!("ssh-link in {} ended: {message}", self.pod);
        }
        served
    }

    /// Follow the build log into `log` from where `lines` is, until the
    /// build is over and the log was read to its end: until a `tail` that
    /// succeeds has nothing more to send. A stream that breaks is taken up
    /// again from the last byte received.
    ///
    /// Never ends for a build that left no report: whoever knows the build
    /// container ended stops waiting for this.
    pub async fn follow(&self, lines: &Mutex<Lines>, log: &mpsc::Sender<String>) {
        if self.until_followable().await == Followable::Never {
            return std::future::pending().await;
        }
        loop {
            match self.tail(lines, log).await {
                // Only a tail that had nothing more to send says the log was
                // read to its end: one that sent something may have been cut
                // short and still succeeded, so another follows it from
                // where it stopped.
                Tail::Ended { bytes: 0 } => return,
                Tail::Ended { .. } => {}
                Tail::Broken { message } => {
                    tracing::debug!("following the log of {} broke: {message}", self.pod);
                    tokio::time::sleep(RESUME_DELAY).await;
                }
            }
        }
    }

    /// Wait for `hold` to run, which is the container the log is read
    /// through.
    async fn until_followable(&self) -> Followable {
        let mut generation = self.cluster.generation();
        loop {
            let followable = self
                .cluster
                .pod(self.pod)
                .is_some_and(|pod| match observe(&pod) {
                    Observed::Building { hold_runs, .. } => hold_runs,
                    Observed::Built(_) => true,
                    _ => false,
                });
            if followable {
                return Followable::Now;
            }
            if generation.changed().await.is_err() {
                return Followable::Never;
            }
        }
    }

    /// One `tail` from where `lines` is, to the end of its stream.
    async fn tail(&self, lines: &Mutex<Lines>, log: &mpsc::Sender<String>) -> Tail {
        let from = lock(lines).received().to_string();
        let argv = [self.bin, "tail", "--work", WORK, "--from", &from];
        let exec = match self
            .cluster
            .exec(self.pod, HOLD, &argv, Stdin::Closed)
            .await
        {
            Ok(exec) => exec,
            Err(error) => {
                return Tail::Broken {
                    message: format!("{error:#}"),
                };
            }
        };
        let Exec {
            mut stdout, ended, ..
        } = exec;
        let mut chunk = vec![0; CHUNK];
        let mut bytes = 0u64;
        loop {
            match stdout.read(&mut chunk).await {
                Ok(0) => break,
                Ok(read) => {
                    bytes += read as u64;
                    let text = lock(lines).feed(&chunk[..read]);
                    if let Some(text) = text {
                        let _ = log.send(text).await;
                    }
                }
                Err(error) => {
                    return Tail::Broken {
                        message: error.to_string(),
                    };
                }
            }
        }
        match ended.await {
            ExecEnd::Succeeded => Tail::Ended { bytes },
            ExecEnd::Failed { message } | ExecEnd::Lost { message } => Tail::Broken { message },
        }
    }

    /// What the build left: its report, and each package's name, size and
    /// digest.
    pub async fn manifest(&self) -> Result<Handback> {
        let argv = [self.bin, "manifest", "--work", WORK];
        let Exec { stdout, ended, .. } = self
            .cluster
            .exec(self.pod, HOLD, &argv, Stdin::Closed)
            .await?;
        let mut text = Vec::new();
        stdout
            .take(MANIFEST_LIMIT + 1)
            .read_to_end(&mut text)
            .await
            .context("reading the manifest")?;
        ensure!(
            text.len() as u64 <= MANIFEST_LIMIT,
            "the manifest is larger than {MANIFEST_LIMIT} bytes"
        );
        match ended.await {
            ExecEnd::Succeeded => {}
            ExecEnd::Failed { message } | ExecEnd::Lost { message } => bail!("{message}"),
        }
        let handback: Handback =
            serde_json::from_slice(&text).context("the manifest is not one the agent writes")?;
        ensure!(
            handback.interface == INTERFACE,
            "the pod's agent speaks interface {}, and this worker interface {INTERFACE}: the \
             agent image is not this worker's",
            handback.interface
        );
        Ok(handback)
    }

    /// Send one package from the pod to `uploads`, holding none of it.
    ///
    /// A file the server refuses for what it is fails at once, as a
    /// [`PackagesRefused`]. Anything else -- a stream that broke, a count or
    /// a digest that is not the manifest's -- is tried once more from a fresh
    /// `send`, which overwrites what the first left on the server.
    pub async fn relay<U: Uploads>(
        &self,
        uploads: &U,
        build_id: i32,
        artifact: &Artifact,
    ) -> Result<()> {
        ensure!(
            is_artifact(&artifact.name),
            "the pod's manifest names {:?}, which is not a package",
            artifact.name
        );
        let first = match self.send(uploads, build_id, artifact).await {
            Ok(()) => return Ok(()),
            Err(error) if error.downcast_ref::<PackagesRefused>().is_some() => return Err(error),
            Err(error) => error,
        };
        tracing::warn!(
            "build {build_id}: sending {} failed, trying once more: {first:#}",
            artifact.name
        );
        self.send(uploads, build_id, artifact)
            .await
            .with_context(|| format!("again, after a first attempt failed ({first:#})"))
    }

    /// One `send` into one upload, checked against the manifest.
    async fn send<U: Uploads>(
        &self,
        uploads: &U,
        build_id: i32,
        artifact: &Artifact,
    ) -> Result<()> {
        let argv = [
            self.bin,
            "send",
            "--work",
            WORK,
            "--name",
            artifact.name.as_str(),
        ];
        let Exec { stdout, ended, .. } = self
            .cluster
            .exec(self.pod, HOLD, &argv, Stdin::Closed)
            .await?;
        let passing = Arc::new(Mutex::new(Passing::new(stdout)));
        uploads
            .upload(
                build_id,
                &artifact.name,
                Body(Arc::clone(&passing)),
                artifact.size,
            )
            .await?;
        // An upload takes the bytes it declared and may stop there; the
        // stream is read on to its end, so that anything past them is seen
        // and the process is never left writing into a closed pipe.
        let finish = async {
            let tally = Body(passing).finish(artifact.size).await?;
            match ended.await {
                ExecEnd::Succeeded => Ok(tally),
                ExecEnd::Failed { message } | ExecEnd::Lost { message } => {
                    Err(anyhow!("the pod's send ended badly: {message}"))
                }
            }
        };
        let tally = tokio::time::timeout(SEND_END, finish)
            .await
            .map_err(|_| anyhow!("the pod's send did not end after its bytes were taken"))??;
        // A send that succeeded may still have been cut short (see
        // [`ExecEnd::Succeeded`]): the count against the manifest is what
        // says the file arrived whole.
        tally.check(artifact)
    }
}

/// Whether the log can be followed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Followable {
    Now,
    /// The watch on pods is over, so nothing will ever say when.
    Never,
}

/// How one `tail` ended.
enum Tail {
    /// It exited 0 having sent `bytes` bytes: the build is over and the log
    /// was read to its end, unless the stream was cut short and still ended
    /// in success (see [`ExecEnd::Succeeded`]).
    Ended { bytes: u64 },
    /// The stream ended some other way; what was read still counts.
    Broken { message: String },
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The build log as it arrives, cut into text at line ends.
///
/// The count of bytes received is where a broken stream is taken up again,
/// and it does not depend on what was passed on: bytes held back for the rest
/// of their line are kept across streams, so a stream that breaks in the
/// middle of a line, or of a character, loses and repeats nothing.
#[derive(Debug, Default)]
pub struct Lines {
    received: u64,
    held: Vec<u8>,
}

impl Lines {
    /// How many bytes of the log have arrived: the offset the next stream
    /// starts from.
    #[must_use]
    pub fn received(&self) -> u64 {
        self.received
    }

    /// Take `bytes` of the log, and give back the whole lines now at hand.
    pub fn feed(&mut self, bytes: &[u8]) -> Option<String> {
        self.received += bytes.len() as u64;
        self.held.extend_from_slice(bytes);
        let cut = match self.held.iter().rposition(|byte| *byte == b'\n') {
            Some(newline) => newline + 1,
            None if self.held.len() >= LINE_CAP => whole_characters(&self.held),
            None => return None,
        };
        let text = String::from_utf8_lossy(&self.held[..cut]).into_owned();
        self.held.drain(..cut);
        Some(text)
    }

    /// What is held back for a line end that will not come.
    pub fn flush(&mut self) -> Option<String> {
        if self.held.is_empty() {
            return None;
        }
        let text = String::from_utf8_lossy(&self.held).into_owned();
        self.held.clear();
        Some(text)
    }
}

/// How much of `bytes` can be cut off without cutting a character in two:
/// all of it, unless it ends in the start of one.
fn whole_characters(bytes: &[u8]) -> usize {
    match std::str::from_utf8(bytes) {
        Err(error) if error.error_len().is_none() && error.valid_up_to() > 0 => error.valid_up_to(),
        _ => bytes.len(),
    }
}

/// A package on its way through: the stream it comes from, and what has
/// passed so far.
struct Passing {
    stdout: Pin<Box<dyn AsyncRead + Send>>,
    digest: Sha256,
    bytes: u64,
}

impl Passing {
    fn new(stdout: Pin<Box<dyn AsyncRead + Send>>) -> Self {
        Self {
            stdout,
            digest: Sha256::new(),
            bytes: 0,
        }
    }
}

/// What passed, once the stream is over.
struct Tally {
    bytes: u64,
    sha256: String,
}

impl Tally {
    /// Fail unless this is exactly the file the manifest describes.
    fn check(&self, artifact: &Artifact) -> Result<()> {
        ensure!(
            self.bytes == artifact.size,
            "{} arrived as {} bytes, and the pod's manifest says {}",
            artifact.name,
            self.bytes,
            artifact.size
        );
        ensure!(
            self.sha256.eq_ignore_ascii_case(&artifact.sha256),
            "{} arrived with SHA-256 {}, and the pod's manifest says {}",
            artifact.name,
            self.sha256,
            artifact.sha256
        );
        Ok(())
    }
}

/// A handle on a package passing through, for whoever reads it next: the
/// upload first, then the relay itself for whatever the upload left.
struct Body(Arc<Mutex<Passing>>);

impl AsyncRead for Body {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let mut passing = lock(&self.0);
        let before = buf.filled().len();
        ready!(passing.stdout.as_mut().poll_read(cx, buf))?;
        let passed = &buf.filled()[before..];
        passing.digest.update(passed);
        passing.bytes += passed.len() as u64;
        Poll::Ready(Ok(()))
    }
}

impl Body {
    /// Read the stream to its end and say what passed in all. Gives up as
    /// soon as more than `expected` bytes have: nothing past them is a
    /// package's.
    async fn finish(mut self, expected: u64) -> Result<Tally> {
        let mut chunk = vec![0; CHUNK];
        loop {
            let passed = lock(&self.0).bytes;
            ensure!(
                passed <= expected,
                "the pod sent more than the {expected} bytes its manifest says"
            );
            if self.read(&mut chunk).await.context("reading the package")? == 0 {
                break;
            }
        }
        let mut passing = lock(&self.0);
        Ok(Tally {
            bytes: passing.bytes,
            sha256: hex::encode(std::mem::take(&mut passing.digest).finalize()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fed(lines: &mut Lines, bytes: &[u8]) -> String {
        lines.feed(bytes).unwrap_or_default()
    }

    #[test]
    fn whole_lines_pass_and_the_rest_waits() {
        let mut lines = Lines::default();
        assert_eq!(fed(&mut lines, b"one\ntwo\nthr"), "one\ntwo\n");
        assert_eq!(lines.received(), 11);
        assert_eq!(lines.feed(b"e"), None);
        assert_eq!(fed(&mut lines, b"e\n"), "three\n");
        assert_eq!(lines.received(), 14);
        assert_eq!(lines.flush(), None);
    }

    /// The offset counts what arrived, not what was passed on, so a stream
    /// taken up again there continues the held-back line.
    #[test]
    fn a_line_cut_by_a_broken_stream_is_joined_by_the_next() {
        let mut lines = Lines::default();
        assert_eq!(lines.feed("caf\u{e9}".as_bytes().split_at(4).0), None);
        assert_eq!(lines.received(), 4);
        assert_eq!(fed(&mut lines, b"\xa9 au lait\n"), "caf\u{e9} au lait\n");
    }

    #[test]
    fn what_is_left_without_a_line_end_is_flushed() {
        let mut lines = Lines::default();
        assert_eq!(lines.feed(b"no newline"), None);
        assert_eq!(lines.flush().as_deref(), Some("no newline"));
        assert_eq!(lines.flush(), None);
        assert_eq!(lines.received(), 10);
    }

    /// A flood with no line end is passed on in pieces, never cutting a
    /// character in two.
    #[test]
    fn a_line_without_end_is_passed_on_at_the_cap() {
        let mut lines = Lines::default();
        let mut flood = vec![b'x'; LINE_CAP - 1];
        flood.extend_from_slice(&"\u{e9}".as_bytes()[..1]);
        let text = fed(&mut lines, &flood);
        assert_eq!(text.len(), LINE_CAP - 1);
        assert_eq!(fed(&mut lines, b"\xa9\n"), "\u{e9}\n");
    }

    #[test]
    fn bytes_that_are_not_text_do_not_stall_the_log() {
        let mut lines = Lines::default();
        let text = fed(&mut lines, &vec![0xff; LINE_CAP]);
        assert_eq!(text.chars().count(), LINE_CAP);
        assert_eq!(lines.flush(), None);
    }

    fn artifact(bytes: &[u8]) -> Artifact {
        Artifact {
            name: "p-1-1-x86_64.pkg.tar.zst".to_string(),
            size: bytes.len() as u64,
            sha256: hex::encode(Sha256::digest(bytes)),
        }
    }

    fn body(bytes: &'static [u8]) -> Body {
        Body(Arc::new(Mutex::new(Passing::new(Box::pin(bytes)))))
    }

    /// Whoever read which part, the tally is of the whole stream.
    #[tokio::test]
    async fn the_tally_covers_what_the_upload_read_and_what_it_left() {
        let bytes = b"0123456789";
        let mut upload = body(bytes);
        let rest = Body(Arc::clone(&upload.0));
        let mut taken = [0; 4];
        upload.read_exact(&mut taken).await.unwrap();
        let tally = rest.finish(10).await.unwrap();
        tally.check(&artifact(bytes)).unwrap();
    }

    #[tokio::test]
    async fn a_stream_that_is_not_the_manifests_file_is_refused() {
        let expected = artifact(b"0123456789");
        let short = body(b"01234").finish(10).await.unwrap();
        let message = short.check(&expected).unwrap_err().to_string();
        assert!(message.contains("5 bytes"), "{message}");

        let other = body(b"0123456780").finish(10).await.unwrap();
        let message = other.check(&expected).unwrap_err().to_string();
        assert!(message.contains("SHA-256"), "{message}");

        let long = body(b"0123456789 and more").finish(10).await;
        assert!(long.is_err());
    }
}
