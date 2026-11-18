//! Running one command to its end, with its output in the build log or kept
//! aside.

use super::children;
use super::output::Output;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::os::fd::FromRawFd;
use std::process::{Command, ExitStatus, Stdio};
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

/// Run a command with no input to its end, or for `limit` at most, keeping
/// what it writes.
///
/// It writes into an unnamed file in memory, not a pipe, so that what it
/// wrote can be read once it has ended whatever else still holds the file: a
/// process it left behind would keep a pipe from ever reaching its end.
pub fn captured(
    command: &mut Command,
    keep: Keep,
    limit: Option<Duration>,
) -> io::Result<Captured> {
    let mut aside = memory_file()?;
    command.stdin(Stdio::null()).stdout(aside.try_clone()?);
    match keep {
        Keep::Both => command.stderr(aside.try_clone()?),
        Keep::Stdout => command.stderr(Stdio::null()),
    };
    let running = children::spawn(command)?;
    let status = match limit {
        None => Some(children::wait(running.pid)),
        Some(limit) => children::wait_within(running.pid, limit).or_else(|| {
            children::kill(running.pid);
            None
        }),
    };
    let mut bytes = Vec::new();
    aside.seek(SeekFrom::Start(0))?;
    aside.take(KEPT).read_to_end(&mut bytes)?;
    Ok(Captured {
        status,
        text: String::from_utf8_lossy(&bytes).into_owned(),
    })
}

/// What a command printed, and how it ended.
pub struct Printed {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
}

/// Run a command with no input to its end, with its stdout kept aside and
/// its stderr in the build log, and read back at most `max` bytes of what it
/// printed: more than that is an error.
pub fn printed(output: &Output, command: &mut Command, max: u64) -> io::Result<Printed> {
    let mut aside = memory_file()?;
    command
        .stdin(Stdio::null())
        .stdout(aside.try_clone()?)
        .stderr(output.stdio()?);
    let running = children::spawn(command)?;
    let status = children::wait(running.pid);
    let mut stdout = Vec::new();
    aside.seek(SeekFrom::Start(0))?;
    (&mut aside).take(max + 1).read_to_end(&mut stdout)?;
    if stdout.len() as u64 > max {
        return Err(io::Error::other(format!(
            "it printed more than {max} bytes"
        )));
    }
    Ok(Printed { status, stdout })
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
