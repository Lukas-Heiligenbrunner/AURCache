//! Where a build's output goes: the agent's stdout, and the log file when
//! one was asked for.
//!
//! One pipe carries everything. Each child gets its write end for stdout and
//! stderr, the agent writes its own lines into it too, and one thread copies
//! what comes out to both places, so the log is in the order things were
//! written. No child's end of output is waited for separately: a build may
//! leave a process holding the pipe, and the copy only ends once the agent
//! has killed everything and closed its own end.

use std::fs::File;
use std::io::{self, PipeReader, PipeWriter, Read, Write};
use std::process::Stdio;
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

/// What starts each of the agent's own lines.
const PREFIX: &str = "[agent]";

/// How long closing waits for the copy to finish: it ends as soon as nothing
/// holds the pipe, which the caller has seen to.
const DRAIN: Duration = Duration::from_secs(30);

pub struct Output {
    /// The write end, until the output is closed.
    pipe: Mutex<Option<PipeWriter>>,
    /// The log file again, for what is said after the pipe is closed.
    log: Mutex<Option<File>>,
    /// Hears from the copying thread when it has written everything.
    drained: Mutex<Receiver<()>>,
}

impl Output {
    /// Start copying to stdout and, when there is one, to `log`.
    pub fn open(log: Option<File>) -> io::Result<Self> {
        let (reader, writer) = io::pipe()?;
        let copy_log = log.as_ref().map(File::try_clone).transpose()?;
        let (done, drained) = channel();
        std::thread::spawn(move || {
            copy(reader, copy_log);
            let _ = done.send(());
        });
        Ok(Self {
            pipe: Mutex::new(Some(writer)),
            log: Mutex::new(log),
            drained: Mutex::new(drained),
        })
    }

    /// The pipe, for a child's stdout or stderr.
    pub fn stdio(&self) -> io::Result<Stdio> {
        let pipe = self.pipe.lock().unwrap_or_else(PoisonError::into_inner);
        let writer = pipe
            .as_ref()
            .ok_or_else(|| io::Error::other("the build's output is closed"))?;
        Ok(Stdio::from(writer.try_clone()?))
    }

    /// One line of the agent's own.
    pub fn say(&self, line: &str) {
        self.relay(&format!("{PREFIX} {line}\n"));
    }

    /// Text that is already whole lines, as it is.
    pub fn relay(&self, text: &str) {
        let pipe = self.pipe.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(mut writer) = pipe.as_ref() {
            let _ = writer.write_all(text.as_bytes());
            return;
        }
        let mut log = self.log.lock().unwrap_or_else(PoisonError::into_inner);
        write(text.as_bytes(), log.as_mut());
    }

    /// Stop taking output, and return once everything written is in the log.
    ///
    /// Call it once nothing else can hold the pipe, which is when every
    /// other process is dead: the copy ends at the pipe's end of file.
    pub fn close(&self) {
        let writer = self
            .pipe
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if writer.is_none() {
            return;
        }
        drop(writer);
        let drained = self.drained.lock().unwrap_or_else(PoisonError::into_inner);
        let _ = drained.recv_timeout(DRAIN);
    }
}

/// Copy the pipe to both places until its end.
fn copy(mut reader: PipeReader, mut log: Option<File>) {
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => return,
            Ok(n) => write(&buffer[..n], log.as_mut()),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return,
        }
    }
}

/// Write to stdout and the log. A failure of either is not the build's: a
/// reader that went away or a full disk must not stop the other from being
/// written, nor the build from being reported.
fn write(bytes: &[u8], log: Option<&mut File>) {
    let mut stdout = io::stdout().lock();
    let _ = stdout.write_all(bytes).and_then(|()| stdout.flush());
    if let Some(log) = log {
        let _ = log.write_all(bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::Output;
    use std::io::{Read, Seek, SeekFrom};
    use std::process::Command;

    /// A child's output and the agent's own lines reach the log in the order
    /// they were written, and all of it is there once the output is closed.
    #[test]
    fn the_log_holds_everything_in_order_once_closed() {
        let mut log = tempfile::tempfile().unwrap();
        let output = Output::open(Some(log.try_clone().unwrap())).unwrap();
        output.say("before");
        let status = Command::new("sh")
            .args(["-c", "echo out; echo err >&2"])
            .stdout(output.stdio().unwrap())
            .stderr(output.stdio().unwrap())
            .status()
            .unwrap();
        assert!(status.success());
        output.say("after");
        output.close();
        output.say("late");

        let mut text = String::new();
        log.seek(SeekFrom::Start(0)).unwrap();
        log.read_to_string(&mut text).unwrap();
        assert_eq!(
            text,
            "[agent] before\nout\nerr\n[agent] after\n[agent] late\n"
        );
        assert!(output.stdio().is_err());
    }
}
