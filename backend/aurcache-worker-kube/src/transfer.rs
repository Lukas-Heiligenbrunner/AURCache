//! The exchanges with a build pod, each one `exec` of the agent: handing the
//! job in (`receive`), following the log (`tail`), and handing the packages
//! back (`manifest`, then one `send` per package relayed straight into its
//! upload).
//!
//! Nothing here holds a package: no temporary file, no buffer of a whole file.
//! A package's bytes go from the exec's bounded pipe into the upload's request
//! body as the upload asks for them, and are counted on the way.

use crate::cluster::{Cluster, Exec, ExecEnd, Stdin};
use crate::pod::{self, BUILD, HOLD, STAGE, WORK};
use anyhow::{Context, Result, anyhow, bail, ensure};
use aurcache_build_agent::report::{Artifact, Handback};
use aurcache_build_agent::spec::INTERFACE;
use aurcache_worker_container::input::Packed;
use aurcache_worker_container::ssh::link;
use aurcache_worker_core::client::WorkerClient;
use aurcache_worker_core::protocol::PackagesRefused;
use std::convert::Infallible;
use std::path::Path;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio_util::io::InspectReader;

/// The most a manifest may be. It is a line per package and a report of a
/// few lines; anything near this is not one.
const MANIFEST_LIMIT: u64 = 4 * 1024 * 1024;
/// How long `manifest` may take. It lists the packages and reads the
/// report, a moment's work: this is for a pod that does not answer.
const MANIFEST_TIME: Duration = Duration::from_secs(5 * 60);
/// The most log held back while waiting for the end of a line.
const LINE_CAP: usize = 64 * 1024;
/// How long to wait before following the log again after a `tail` ended.
const RESUME_DELAY: Duration = Duration::from_secs(2);
/// How long a `send` may pass no byte before it is given up: the upload has
/// no deadline of its own, and a stalled one would hold its build's slot.
const SEND_IDLE: Duration = Duration::from_secs(5 * 60);
/// How much of the log is read at once.
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
    /// Hand the job in: `input` is what `receive` reads, written a piece at
    /// a time.
    ///
    /// Not to be tried twice: `receive` refuses a job that was already handed
    /// in, and a build may have started on the first.
    pub async fn receive(&self, input: &Packed) -> Result<()> {
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
            for piece in input.pieces() {
                stdin.write_all(piece).await?;
            }
            stdin.shutdown().await
        };
        // Read while writing: a process that says something before it has
        // read everything must not be left waiting on a full pipe.
        let mut nowhere = tokio::io::sink();
        let drain = tokio::io::copy(&mut stdout, &mut nowhere);
        let (written, _) = tokio::join!(write, drain);
        match ended.await {
            // `receive` exits once it has read both inputs, each as long as
            // its length says, which may be before this end of its stdin was
            // closed: only its own word says whether the job arrived.
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
    pub async fn ssh_link(&self, ssh_agent: &Path, socket: &Path) -> Result<()> {
        let bin = pod::build_agent_path();
        let socket = socket.to_string_lossy();
        let argv = [bin.as_str(), "ssh-link", "--socket", &socket];
        let Exec {
            stdin,
            stdout,
            ended,
        } = self
            .cluster
            .exec(self.pod, BUILD, &argv, Stdin::Piped)
            .await
            .context("the exec of ssh-link")?;
        let stdin = stdin.context("the exec of ssh-link gave no stdin")?;
        let served = link::serve(ssh_agent, stdout, stdin).await;
        if let ExecEnd::Failed { message } | ExecEnd::Lost { message } = ended.await {
            tracing::debug!("ssh-link in {} ended: {message}", self.pod);
        }
        Ok(served?)
    }

    /// Follow the build log into `log` from where `lines` is, taking a
    /// broken stream up again from the last byte received. Never ends:
    /// whoever knows the build container ended drops it, and reads the rest
    /// with [`Self::follow_to`].
    pub async fn follow(&self, lines: &Mutex<Lines>, log: &mpsc::Sender<String>) -> Infallible {
        loop {
            if let Err(error) = self.tail(lines, log, u64::MAX).await {
                tracing::debug!("following the log of {} broke: {error:#}", self.pod);
            }
            tokio::time::sleep(RESUME_DELAY).await;
        }
    }

    /// Follow the build log until `size` bytes of it, its whole length as
    /// the manifest gives it, have arrived. A `tail` may end in success
    /// short of that (see [`ExecEnd::Succeeded`]), so only the count says
    /// the log is whole.
    pub async fn follow_to(&self, lines: &Mutex<Lines>, log: &mpsc::Sender<String>, size: u64) {
        while lock(lines).received() < size {
            if let Err(error) = self.tail(lines, log, size).await {
                tracing::debug!("reading the log of {} broke: {error:#}", self.pod);
            }
            if lock(lines).received() < size {
                tokio::time::sleep(RESUME_DELAY).await;
            }
        }
    }

    /// One `tail` from where `lines` is, until its stream ends or `until`
    /// bytes of the log have arrived.
    ///
    /// Safe to drop at any await: bytes count as received only once they
    /// are passed on, and the next `tail` starts from there.
    async fn tail(
        &self,
        lines: &Mutex<Lines>,
        log: &mpsc::Sender<String>,
        until: u64,
    ) -> Result<()> {
        let from = lock(lines).received();
        let from_arg = from.to_string();
        let argv = [self.bin, "tail", "--work", WORK, "--from", &from_arg];
        let Exec { stdout, ended, .. } = self
            .cluster
            .exec(self.pod, HOLD, &argv, Stdin::Closed)
            .await?;
        let mut stdout = stdout.take(until.saturating_sub(from));
        let mut chunk = vec![0; CHUNK];
        loop {
            let read = stdout.read(&mut chunk).await?;
            if read == 0 {
                break;
            }
            let permit = log.reserve().await.ok();
            let text = lock(lines).feed(&chunk[..read]);
            if let (Some(permit), Some(text)) = (permit, text) {
                permit.send(text);
            }
        }
        if lock(lines).received() >= until {
            return Ok(());
        }
        match ended.await {
            ExecEnd::Succeeded => Ok(()),
            ExecEnd::Failed { message } | ExecEnd::Lost { message } => bail!("{message}"),
        }
    }

    /// What the build left: its report, its log's length, and each
    /// package's name and size, within [`MANIFEST_TIME`].
    pub async fn manifest(&self) -> Result<Handback> {
        tokio::time::timeout(MANIFEST_TIME, self.read_manifest())
            .await
            .map_err(|_| {
                anyhow!(
                    "the pod did not say what the build left within {} minutes",
                    MANIFEST_TIME.as_secs() / 60
                )
            })?
    }

    async fn read_manifest(&self) -> Result<Handback> {
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
    /// [`PackagesRefused`]. Anything else -- a stream that broke, a count
    /// that is not the manifest's -- is tried once more from a fresh
    /// `send`, which overwrites what the first left on the server.
    pub async fn relay<U: Uploads>(
        &self,
        uploads: &U,
        build_id: i32,
        artifact: &Artifact,
    ) -> Result<()> {
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

    /// One `send` into one upload, checked against the manifest, and given
    /// up once no byte has passed for [`SEND_IDLE`].
    async fn send<U: Uploads>(
        &self,
        uploads: &U,
        build_id: i32,
        artifact: &Artifact,
    ) -> Result<()> {
        // One word, so that a name starting with `-` is not taken for a flag.
        let name = format!("--name={}", artifact.name);
        let argv = [self.bin, "send", "--work", WORK, name.as_str()];
        let Exec { stdout, ended, .. } = self
            .cluster
            .exec(self.pod, HOLD, &argv, Stdin::Closed)
            .await?;
        let passed = Arc::new(AtomicU64::new(0));
        let body = counted(stdout, Arc::clone(&passed));
        let relay = async {
            uploads
                .upload(build_id, &artifact.name, body, artifact.size)
                .await?;
            match ended.await {
                ExecEnd::Succeeded => Ok(()),
                ExecEnd::Failed { message } | ExecEnd::Lost { message } => {
                    Err(anyhow!("the pod's send ended badly: {message}"))
                }
            }
        };
        tokio::select! {
            relayed = relay => relayed?,
            () = stalled(&passed) => bail!(
                "no byte of {} passed for {} minutes",
                artifact.name,
                SEND_IDLE.as_secs() / 60
            ),
        }
        // The upload declares the manifest's size, and a body that ends
        // short of it fails the request. A send that succeeded may still
        // have been cut short (see [`ExecEnd::Succeeded`]), so the count
        // is checked too.
        let bytes = passed.load(Ordering::Relaxed);
        ensure!(
            bytes == artifact.size,
            "{} arrived as {bytes} bytes, and the pod's manifest says {}",
            artifact.name,
            artifact.size
        );
        Ok(())
    }
}

/// `stdout`, adding to `passed` each byte read from it.
fn counted(
    stdout: Pin<Box<dyn AsyncRead + Send>>,
    passed: Arc<AtomicU64>,
) -> impl AsyncRead + Send + Unpin + 'static {
    InspectReader::new(stdout, move |bytes: &[u8]| {
        passed.fetch_add(bytes.len() as u64, Ordering::Relaxed);
    })
}

/// Resolves once `passed` has not moved for a whole [`SEND_IDLE`].
async fn stalled(passed: &AtomicU64) {
    let mut last = passed.load(Ordering::Relaxed);
    loop {
        tokio::time::sleep(SEND_IDLE).await;
        let now = passed.load(Ordering::Relaxed);
        if now == last {
            return;
        }
        last = now;
    }
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
}
