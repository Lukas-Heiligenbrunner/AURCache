//! Streaming a build's output into its log on the server, batched.
//!
//! A build emits thousands of lines; one request per line would swamp the
//! server, and one request at the end would defeat the purpose. So output is
//! gathered into batches sent on size or age, whichever comes first -- the age
//! on a timer of its own, so the last lines before a long silence (a compile
//! that prints nothing for minutes) are on the page within the interval rather
//! than whenever the next line arrives.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::client::WorkerClient;
use crate::protocol::{Stop, log};

/// How much output to accumulate before sending.
const BATCH_BYTES: usize = 4096;
/// How long a partial batch is held.
const BATCH_INTERVAL: Duration = Duration::from_millis(500);
/// Largest single read from a stream: a newline-less flood must arrive in
/// capped chunks, never as one unbounded `read_until` allocation. Chunk
/// boundaries are invisible downstream -- the log is append-only.
const LINE_CAP_BYTES: usize = 1024 * 1024;
/// How long a stopped build's output may stay open before the build is
/// reported ended anyway. Generous: a killed tree closes its pipes within
/// milliseconds, so reaching this means something escaped the kill, not that
/// it was slow.
const KILLED_OUTPUT_GRACE: Duration = Duration::from_secs(30);
/// Chunks in flight between the streams and the sender. Bounded, so a server
/// that is slow to take the log slows the build's output down rather than
/// filling the worker's memory with it.
const QUEUE: usize = 64;

/// One build's log: output goes in through [`Self::pump`] or
/// [`Self::sender`], and is sent to the server in batches.
pub struct BuildLog {
    tx: mpsc::Sender<String>,
    sender: JoinHandle<()>,
    build_id: i32,
}

impl BuildLog {
    /// Start sending `build_id`'s output to the server.
    #[must_use]
    pub fn open(client: Arc<WorkerClient>, build_id: i32) -> Self {
        let (tx, rx) = mpsc::channel(QUEUE);
        let sender = tokio::spawn(batch(rx, move |text| {
            let client = Arc::clone(&client);
            async move { log(&client, build_id, &text).await }
        }));
        Self {
            tx,
            sender,
            build_id,
        }
    }

    /// Where to send chunks of output that arrive some other way than a
    /// byte stream.
    #[must_use]
    pub fn sender(&self) -> mpsc::Sender<String> {
        self.tx.clone()
    }

    /// Forward `reader` into the log on a task of its own, until it ends.
    pub fn pump<R>(&self, reader: R) -> JoinHandle<()>
    where
        R: tokio::io::AsyncRead + Unpin + Send + 'static,
    {
        tokio::spawn(read_into(reader, self.sender(), self.build_id))
    }

    /// Wait for `pumps` to reach the end of their streams, then send what is
    /// left. Before the report: the last lines are the ones that explain a
    /// failure.
    ///
    /// A stream ends only once *every* holder of it has gone. After a normal
    /// exit that is at once; after a `stop` it may never be, if something
    /// outlived the kill -- a container that escaped the cgroup did exactly
    /// that, and the build stayed active with its lease held until the process
    /// was stopped by hand. So a stopped build waits a bounded time.
    pub async fn close(self, pumps: Vec<JoinHandle<()>>, stop: Option<Stop>) {
        let aborts: Vec<_> = pumps.iter().map(JoinHandle::abort_handle).collect();
        let drained = async {
            for pump in pumps {
                let _ = pump.await;
            }
        };
        if stop.is_none() {
            drained.await;
        } else if tokio::time::timeout(KILLED_OUTPUT_GRACE, drained)
            .await
            .is_err()
        {
            aborts.iter().for_each(tokio::task::AbortHandle::abort);
            tracing::warn!(
                "build {} was stopped but its output is still open after {}s; something \
                 outlived the kill. Reporting it ended regardless",
                self.build_id,
                KILLED_OUTPUT_GRACE.as_secs()
            );
        }
        drop(self.tx);
        let _ = self.sender.await;
    }
}

/// Read `reader` in lines (capped) and queue each for the log.
async fn read_into<R>(reader: R, tx: mpsc::Sender<String>, build_id: i32)
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::{AsyncBufReadExt, AsyncReadExt};

    let mut reader = tokio::io::BufReader::new(reader);
    let mut line = Vec::new();
    loop {
        line.clear();
        // Bytes, decoded lossily: build output is not always UTF-8 (and a
        // capped chunk can end mid-character), and `read_line` would treat
        // either as a read error -- ending the log and leaving the child
        // writing into a pipe nobody drains.
        match (&mut reader)
            .take(LINE_CAP_BYTES as u64)
            .read_until(b'\n', &mut line)
            .await
        {
            Ok(0) => break,
            Ok(_) => {
                if tx
                    .send(String::from_utf8_lossy(&line).into_owned())
                    .await
                    .is_err()
                {
                    break;
                }
            }
            // A read error ends the stream; the build's own exit status is
            // what determines success, so this is not escalated.
            Err(e) => {
                tracing::debug!("build {build_id}: reading output ended: {e}");
                break;
            }
        }
    }
}

/// Gather what arrives on `rx` into batches and hand each to `send`: on size,
/// on age, and whatever is left once every sender has gone.
async fn batch<F, Fut>(mut rx: mpsc::Receiver<String>, send: F)
where
    F: Fn(String) -> Fut,
    Fut: Future<Output = ()>,
{
    let mut pending = String::new();
    // When the oldest output in `pending` has to go; meaningless while empty.
    let mut due = Instant::now();
    loop {
        tokio::select! {
            chunk = rx.recv() => {
                let Some(chunk) = chunk else { break };
                if pending.is_empty() {
                    due = Instant::now() + BATCH_INTERVAL;
                }
                pending.push_str(&chunk);
                if pending.len() >= BATCH_BYTES {
                    send(std::mem::take(&mut pending)).await;
                }
            }
            () = tokio::time::sleep_until(due), if !pending.is_empty() => {
                send(std::mem::take(&mut pending)).await;
            }
        }
    }
    if !pending.is_empty() {
        send(pending).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    type Sent = Arc<Mutex<Vec<String>>>;

    /// A `send` for `batch` that records what it was given into `sent`.
    fn record_into(sent: &Sent) -> impl Fn(String) -> std::future::Ready<()> + use<> {
        let sent = Arc::clone(sent);
        move |text| {
            sent.lock().unwrap().push(text);
            std::future::ready(())
        }
    }

    /// The bug this timer exists for: a line that arrived just after a send
    /// sat in the buffer until the next line, which in a long silent compile
    /// is minutes.
    #[tokio::test(start_paused = true)]
    async fn a_partial_batch_goes_within_the_interval_without_more_output() {
        let sent = Sent::default();
        let (tx, rx) = mpsc::channel(QUEUE);
        let task = tokio::spawn(batch(rx, record_into(&sent)));

        tx.send("compiling...\n".to_string()).await.unwrap();
        tokio::time::sleep(BATCH_INTERVAL / 2).await;
        assert!(sent.lock().unwrap().is_empty(), "sent before it was due");

        tokio::time::sleep(BATCH_INTERVAL).await;
        assert_eq!(*sent.lock().unwrap(), ["compiling...\n"]);

        drop(tx);
        task.await.unwrap();
        assert_eq!(sent.lock().unwrap().len(), 1, "nothing more to send");
    }

    /// Lines arriving together go together, and a full batch goes at once.
    #[tokio::test(start_paused = true)]
    async fn output_is_batched_by_size_and_flushed_at_the_end() {
        let sent = Sent::default();
        let (tx, rx) = mpsc::channel(QUEUE);
        let task = tokio::spawn(batch(rx, record_into(&sent)));

        let big = "x".repeat(BATCH_BYTES);
        tx.send("a\n".to_string()).await.unwrap();
        tx.send(big.clone()).await.unwrap();
        tx.send("tail\n".to_string()).await.unwrap();
        drop(tx);
        task.await.unwrap();

        assert_eq!(
            *sent.lock().unwrap(),
            [format!("a\n{big}"), "tail\n".to_string()]
        );
    }

    /// A stream is read in lines, invalid UTF-8 included, to its end.
    #[tokio::test]
    async fn a_stream_is_read_in_lines_to_its_end() {
        let (tx, mut rx) = mpsc::channel(QUEUE);
        let input: &[u8] = b"one\ntwo \xff\nthree";
        read_into(input, tx, 1).await;
        let mut lines = Vec::new();
        while let Some(line) = rx.recv().await {
            lines.push(line);
        }
        assert_eq!(lines, ["one\n", "two \u{fffd}\n", "three"]);
    }
}
