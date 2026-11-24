//! The build log, and running one command to its end with its output in it
//! or kept aside.
//!
//! The build log is the agent's stdout, which the worker reads, or a file
//! that `--log-file` names. Each child
//! writes its stdout and stderr straight to it, and the agent its own lines,
//! flushed as they are written, so everything is in the order it was written.

use super::children;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::fd::{AsFd, AsRawFd, FromRawFd};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};

/// One line of the agent's own in the build log. A reader that went away
/// must not stop the build from being reported, so a failure is ignored.
pub fn say(line: &str) {
    let mut stdout = io::stdout().lock();
    let _ = writeln!(stdout, "[agent] {line}").and_then(|()| stdout.flush());
}

/// Make the file `path` the build log, in place of stdout, for a worker that
/// follows the log as a file. Root's, and readable by anyone.
pub fn log_to(path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o755)
            .create(parent)?;
    }
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .custom_flags(libc::O_NOFOLLOW)
        .mode(0o644)
        .open(path)?;
    // SAFETY: both descriptors are open, and `dup2` only replaces the second.
    if unsafe { libc::dup2(file.as_raw_fd(), libc::STDOUT_FILENO) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// The build log, for a child's stdout or stderr. It is made blocking again
/// first: an earlier child may have set `O_NONBLOCK` on the description it
/// shares, and writes to it would then fail with `EAGAIN`.
pub fn log() -> io::Result<Stdio> {
    let log = io::stdout().as_fd().try_clone_to_owned()?;
    // SAFETY: `log` is an open descriptor, and only its status flags change.
    unsafe {
        let flags = libc::fcntl(log.as_raw_fd(), libc::F_GETFL);
        if flags >= 0 {
            libc::fcntl(log.as_raw_fd(), libc::F_SETFL, flags & !libc::O_NONBLOCK);
        }
    }
    Ok(Stdio::from(log))
}

/// Run a command to its end, with what it writes in the build log.
pub fn logged(command: &mut Command) -> io::Result<ExitStatus> {
    command.stdin(Stdio::null()).stdout(log()?).stderr(log()?);
    children::wait(children::spawn(command)?)
}

/// What a command printed, and how it ended.
pub struct Printed {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
}

/// Run a command with no input to its end, with its stdout kept aside and
/// its stderr in the build log, and read back at most `max` bytes of what it
/// printed: more than that is an error.
pub fn printed(command: &mut Command, max: u64) -> io::Result<Printed> {
    let mut aside = memory_file()?;
    command
        .stdin(Stdio::null())
        .stdout(aside.try_clone()?)
        .stderr(log()?);
    let status = children::wait(children::spawn(command)?)?;
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
