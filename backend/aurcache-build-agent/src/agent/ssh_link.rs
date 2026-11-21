//! `ssh-link`: the process that the worker execs into the container to copy
//! the SSH link between the exec's stdin and stdout and the agent's socket.
//! See "The SSH link" in `design/in-progress/container-workers.md`.

use aurcache_build_agent::layout::BIN_NAME;
use std::io::{ErrorKind, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::ExitCode;
use std::time::{Duration, Instant};

/// How long the agent may take to listen: it does so first thing, so this is
/// the time it takes to start.
const PATIENCE: Duration = Duration::from_secs(60);
const RETRY: Duration = Duration::from_millis(100);
const CHUNK: usize = 16 * 1024;

/// Carry the link until either side ends. Exits 1, saying why on stderr,
/// when it could not.
pub fn run(socket: &Path) -> ExitCode {
    match carry(socket) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{BIN_NAME}: {error}");
            ExitCode::FAILURE
        }
    }
}

fn carry(socket: &Path) -> std::io::Result<()> {
    // Root in the container has this process's uid. Undumpable, it cannot
    // be traced or have its memory read from in there, which matters because
    // it is the one process that the agent spares (`children::Spare`).
    // SAFETY: `prctl(PR_SET_DUMPABLE)` has no memory preconditions.
    if unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let stream = connect(socket)?;
    let mut to_agent = stream.try_clone()?;
    // stdin to the socket, on a thread of its own; the process ends when
    // the agent's side does, whatever this thread is doing.
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let _ = copy(&mut stdin, &mut to_agent);
        let _ = to_agent.shutdown(Shutdown::Write);
    });
    let mut from_agent = stream;
    let mut stdout = std::io::stdout().lock();
    copy(&mut from_agent, &mut stdout)
}

/// Connect, waiting for a socket the agent has not opened yet.
fn connect(socket: &Path) -> std::io::Result<UnixStream> {
    let deadline = Instant::now() + PATIENCE;
    loop {
        match UnixStream::connect(socket) {
            Ok(stream) => return Ok(stream),
            Err(e)
                if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::ConnectionRefused)
                    && Instant::now() < deadline =>
            {
                std::thread::sleep(RETRY);
            }
            Err(e) => return Err(e),
        }
    }
}

/// Copy until the reader ends, flushing each chunk: the other end waits on
/// what was written.
fn copy(from: &mut impl Read, to: &mut impl Write) -> std::io::Result<()> {
    let mut chunk = vec![0; CHUNK];
    loop {
        let read = match from.read(&mut chunk) {
            Ok(0) => return Ok(()),
            Ok(read) => read,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        to.write_all(&chunk[..read])?;
        to.flush()?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    #[test]
    fn a_socket_that_comes_late_is_waited_for() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("link.sock");
        let later = path.clone();
        let listening = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            let listener = UnixListener::bind(&later).unwrap();
            listener.accept().unwrap().0
        });
        let mut ours = connect(&path).unwrap();
        let mut theirs = listening.join().unwrap();
        ours.write_all(b"x").unwrap();
        let mut got = [0; 1];
        theirs.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"x");
    }

    #[test]
    fn copy_moves_everything_to_the_end() {
        let data: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        let mut out = Vec::new();
        copy(&mut data.as_slice(), &mut out).unwrap();
        assert_eq!(out, data);
    }
}
