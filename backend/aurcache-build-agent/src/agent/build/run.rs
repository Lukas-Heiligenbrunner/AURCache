//! Running one command to its end, with its output in the build log or kept
//! aside.

use super::children;
use super::output::Output;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::os::fd::FromRawFd;
use std::process::{ChildStdin, Command, ExitStatus, Stdio};
use std::time::Duration;

/// The most that is read back of what a command wrote aside.
const KEPT: u64 = 64 * 1024;

/// How a command whose output was kept aside ended.
pub struct Captured {
    /// `None` when it was killed for running past its time.
    pub status: Option<ExitStatus>,
    /// What it wrote to stdout, and to stderr when that was kept too.
    pub text: String,
}

impl Captured {
    /// Whether it ran to its end and exited with 0.
    pub fn succeeded(&self) -> bool {
        self.status.is_some_and(|status| status.success())
    }
}

/// Which of a command's streams are kept aside.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Keep {
    /// Stdout, with stderr thrown away.
    Stdout,
    /// Both, as one text.
    Both,
}

/// Run a command to its end, with what it writes in the build log.
pub fn logged(output: &Output, command: &mut Command) -> io::Result<ExitStatus> {
    command
        .stdin(Stdio::null())
        .stdout(output.stdio()?)
        .stderr(output.stdio()?);
    let running = children::spawn(command)?;
    Ok(children::wait(running.pid))
}

/// A started command whose output is being kept aside.
pub struct Capturing {
    pid: u32,
    aside: File,
    /// The write end of its stdin, when it was given a pipe there.
    pub stdin: Option<ChildStdin>,
}

impl Capturing {
    /// Wait for the command to end, or for `limit` at most, and read back
    /// what it wrote.
    pub fn finish(mut self, limit: Option<Duration>) -> io::Result<Captured> {
        drop(self.stdin.take());
        let status = match limit {
            None => Some(children::wait(self.pid)),
            Some(limit) => children::wait_within(self.pid, limit).or_else(|| {
                children::kill(self.pid);
                None
            }),
        };
        let mut bytes = Vec::new();
        self.aside.seek(SeekFrom::Start(0))?;
        self.aside.take(KEPT).read_to_end(&mut bytes)?;
        Ok(Captured {
            status,
            text: String::from_utf8_lossy(&bytes).into_owned(),
        })
    }
}

/// Start a command with what it writes kept aside.
///
/// It writes into an unnamed file in memory, not a pipe, so that what it
/// wrote can be read once it has ended whatever else still holds the file: a
/// process it left behind would keep a pipe from ever reaching its end.
pub fn capture(command: &mut Command, keep: Keep) -> io::Result<Capturing> {
    let aside = memory_file()?;
    command.stdout(aside.try_clone()?);
    match keep {
        Keep::Both => command.stderr(aside.try_clone()?),
        Keep::Stdout => command.stderr(Stdio::null()),
    };
    let running = children::spawn(command)?;
    Ok(Capturing {
        pid: running.pid,
        aside,
        stdin: running.stdin,
    })
}

/// Run a command with no input to its end, or for `limit` at most, keeping
/// what it writes.
pub fn captured(
    command: &mut Command,
    keep: Keep,
    limit: Option<Duration>,
) -> io::Result<Captured> {
    command.stdin(Stdio::null());
    capture(command, keep)?.finish(limit)
}

/// How a command ended, in words, for a failure's message.
pub fn describe(status: ExitStatus) -> String {
    use std::os::unix::process::ExitStatusExt;
    match (status.code(), status.signal()) {
        (Some(code), _) => format!("exited with status {code}"),
        (None, Some(signal)) => format!("was killed by signal {signal}"),
        (None, None) => "ended abnormally".to_string(),
    }
}

/// A file that exists only in memory and has no name.
fn memory_file() -> io::Result<File> {
    // SAFETY: the name is a valid C string, and the descriptor returned is
    // owned by nothing else.
    let fd = unsafe { libc::memfd_create(c"aurcache-build-agent".as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is open and this is its only owner.
    Ok(unsafe { File::from_raw_fd(fd) })
}
