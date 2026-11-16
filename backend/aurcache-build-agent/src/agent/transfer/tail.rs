//! Follow the build log from a given byte, until the build is over.

use super::file::{pump, stdout};
use anyhow::{Context, Result, bail};
use aurcache_build_agent::layout::Work;
use std::fs::{self, File};
use std::io::{self, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::path::PathBuf;

/// How long to wait for the log to grow before looking again. The same wait
/// is how a reader that has gone away is noticed while nothing is written.
const WAIT_MS: libc::c_int = 100;

/// What one look at the log came to.
#[derive(Debug, PartialEq, Eq)]
enum Pass {
    /// Bytes were written and the build is still going: look again at once.
    Moved,
    /// Nothing new, and the build is still going.
    Waiting,
    /// The build is over and the log is written to its end.
    Over,
}

/// Where the log is, how far it was written, and what ends the following.
struct Follower {
    log: PathBuf,
    report: PathBuf,
    /// The log, once it exists, positioned at the next byte to write.
    file: Option<File>,
    from: u64,
}

impl Follower {
    fn new(work: &Work, from: u64) -> Self {
        Self {
            log: work.log(),
            report: work.report(),
            file: None,
            from,
        }
    }

    /// Write whatever the log holds beyond what was written, to `to`.
    ///
    /// Whether the build is over is asked before the log is read, so a log
    /// that was complete when the report appeared is read to its end.
    fn pass(&mut self, to: &mut impl Write) -> Result<Pass> {
        let over = fs::symlink_metadata(&self.report).is_ok();
        if self.file.is_none() {
            self.file = self.open()?;
        }
        let written = match &mut self.file {
            Some(file) => pump(file, to).context("cannot write the log")?,
            None => 0,
        };
        Ok(match (over, written) {
            (true, _) => Pass::Over,
            (false, 0) => Pass::Waiting,
            (false, _) => Pass::Moved,
        })
    }

    /// The log at the offset, or `None` while it does not exist yet.
    fn open(&self) -> Result<Option<File>> {
        let mut file = match File::open(&self.log) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error).with_context(|| format!("cannot open {}", self.log.display()));
            }
        };
        file.seek(SeekFrom::Start(self.from))
            .context("cannot seek in the log")?;
        Ok(Some(file))
    }
}

/// Write the build log from byte `from` to stdout, following it.
pub fn run(work: &Work, from: u64) -> Result<()> {
    let mut follower = Follower::new(work, from);
    let mut out = stdout();
    loop {
        match follower.pass(&mut *out)? {
            Pass::Moved => {}
            Pass::Waiting => wait(&out)?,
            Pass::Over => return Ok(()),
        }
    }
}

/// Sleep [`WAIT_MS`], or fail at once if nobody reads `out` any more.
fn wait(out: &File) -> Result<()> {
    let mut watch = libc::pollfd {
        fd: out.as_raw_fd(),
        // Only the conditions poll always reports: a closed reader.
        events: 0,
        revents: 0,
    };
    // SAFETY: `watch` is one valid, initialised pollfd.
    let ready = unsafe { libc::poll(&raw mut watch, 1, WAIT_MS) };
    if ready > 0 && watch.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
        bail!("the reader of the log went away");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Follower, Pass};
    use aurcache_build_agent::layout::Work;
    use std::io::Write;

    fn setup() -> (tempfile::TempDir, Work) {
        let dir = tempfile::tempdir().unwrap();
        let work = Work::new(dir.path());
        std::fs::create_dir_all(work.log().parent().unwrap()).unwrap();
        std::fs::create_dir_all(work.out()).unwrap();
        (dir, work)
    }

    #[test]
    fn follows_appends_from_the_offset_until_the_report() {
        let (_dir, work) = setup();
        let mut follower = Follower::new(&work, 3);
        let mut out = Vec::new();
        assert_eq!(follower.pass(&mut out).unwrap(), Pass::Waiting);
        std::fs::write(work.log(), b"abcdef").unwrap();
        assert_eq!(follower.pass(&mut out).unwrap(), Pass::Moved);
        assert_eq!(follower.pass(&mut out).unwrap(), Pass::Waiting);
        std::fs::OpenOptions::new()
            .append(true)
            .open(work.log())
            .unwrap()
            .write_all(b"ghi")
            .unwrap();
        std::fs::write(work.report(), b"{}").unwrap();
        assert_eq!(follower.pass(&mut out).unwrap(), Pass::Over);
        assert_eq!(out, b"defghi");
    }

    #[test]
    fn a_build_that_never_logged_still_ends_the_tail() {
        let (_dir, work) = setup();
        std::fs::write(work.report(), b"{}").unwrap();
        let mut out = Vec::new();
        assert_eq!(Follower::new(&work, 0).pass(&mut out).unwrap(), Pass::Over);
        assert!(out.is_empty());
    }
}
