//! Follow the build log from an offset, until the build is over.

use aurcache_build_agent::layout::{LOG_DIR, LOG_FILE, Work};
use aurcache_common::beneath::Dir;
use std::fs::File;
use std::io::{self, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;

/// How long to wait for the log to grow before looking again. The same wait
/// is how a reader that went away is noticed while nothing is written.
const WAIT_MS: libc::c_int = 100;

/// Write the build log from byte `from` to stdout, following it, until the
/// report exists and the log was written to its end.
pub fn run(work: &Work, from: u64) -> io::Result<()> {
    let mut out = super::stdout();
    let reader = super::stdout();
    follow(work, from, &mut *out, || wait(&reader))
}

/// [`run`], writing to `to`, with `idle` called whenever there was nothing
/// new.
fn follow(
    work: &Work,
    from: u64,
    to: &mut impl Write,
    mut idle: impl FnMut() -> io::Result<()>,
) -> io::Result<()> {
    let mut log = None;
    loop {
        // Asked before the log is read, so that a log that was complete when
        // the report appeared is read to its end.
        let over = std::fs::symlink_metadata(work.report()).is_ok();
        if log.is_none() {
            log = open(work, from)?;
        }
        let written = match &mut log {
            Some(file) => io::copy(file, to)?,
            None => 0,
        };
        if over {
            return Ok(());
        }
        if written == 0 {
            idle()?;
        }
    }
}

/// The log at `from`, or `None` while there is none yet.
fn open(work: &Work, from: u64) -> io::Result<Option<File>> {
    let opened = Dir::open(work.root())
        .and_then(|root| root.dir(LOG_DIR))
        .and_then(|dir| dir.file(LOG_FILE));
    match opened {
        Ok(mut file) => {
            file.seek(SeekFrom::Start(from))?;
            Ok(Some(file))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Sleep [`WAIT_MS`], or fail at once if nobody reads `out` any more.
fn wait(out: &File) -> io::Result<()> {
    let mut watch = libc::pollfd {
        fd: out.as_raw_fd(),
        // Only the conditions that poll always reports: a closed reader.
        events: 0,
        revents: 0,
    };
    // SAFETY: `watch` is one valid, initialised pollfd.
    let ready = unsafe { libc::poll(&raw mut watch, 1, WAIT_MS) };
    if ready > 0 && watch.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
        return Err(io::Error::other("the reader of the log went away"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::follow;
    use aurcache_build_agent::layout::Work;
    use std::io::Write;

    /// The log is followed from the offset until the report appears, and a
    /// `log/` that is a link to elsewhere is refused.
    #[test]
    fn the_log_is_followed_from_the_offset_until_the_report() {
        let dir = tempfile::tempdir().unwrap();
        let work = Work::new(dir.path());
        std::fs::create_dir_all(work.out()).unwrap();
        let mut out = Vec::new();
        let mut idle = 0;
        follow(&work, 3, &mut out, || {
            idle += 1;
            let mut log = std::fs::OpenOptions::new();
            match idle {
                1 => {
                    std::fs::create_dir(work.log().parent().unwrap()).unwrap();
                    std::fs::write(work.log(), b"abcdef").unwrap();
                }
                2 => {
                    log.append(true)
                        .open(work.log())
                        .unwrap()
                        .write_all(b"ghi")
                        .unwrap();
                    std::fs::write(work.report(), b"{}").unwrap();
                }
                _ => panic!("followed past the report"),
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(out, b"defghi");

        let linked = tempfile::tempdir().unwrap();
        let work = Work::new(linked.path());
        std::os::unix::fs::symlink(dir.path().join("log"), work.log().parent().unwrap()).unwrap();
        assert!(follow(&work, 0, &mut Vec::new(), || Ok(())).is_err());
    }
}
