//! The build's end of the SSH link: root's socket that `ssh-link` connects
//! to, and the build user's agent socket served over it. See "The SSH link"
//! in `design/in-progress/container-workers.md`.
//!
//! Its threads start and wait for no process, so the main thread's
//! `waitpid(-1)` stays the only one (see `children`).

use super::dirs::{self, BUILD, Owner, ROOT};
use aurcache_build_agent::layout::Scratch;
use aurcache_build_agent::ssh_link::{HELLO, message_len};
use std::io::{self, ErrorKind, Read, Write};
use std::net::Shutdown;
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

/// How long the worker has to answer its side of the hello.
const HELLO_TIME: Duration = Duration::from_secs(30);
/// How long one request may take to cross the link, and its answer to come back.
const ANSWER_TIME: Duration = Duration::from_secs(60);
/// The most connections of the build's served at once.
const CONNECTIONS_MAX: usize = 32;

/// Root's socket, waiting for the worker's `ssh-link`.
pub struct Listening {
    listener: UnixListener,
    path: PathBuf,
}

/// The link, once the worker's `ssh-link` connected and said hello.
pub struct Link {
    stream: UnixStream,
    /// The process at its other end: the worker's `ssh-link`.
    pub pid: u32,
}

impl Listening {
    /// Listen, in a directory that only root can enter, before anything of
    /// the job runs.
    pub fn open(scratch: &Scratch) -> io::Result<Self> {
        let path = scratch.ssh_link();
        let dir = path
            .parent()
            .ok_or_else(|| io::Error::other("no directory"))?;
        if let Some(root) = dir.parent() {
            std::fs::create_dir_all(root)?;
            dirs::make(root, ROOT, 0o755)?;
        }
        dirs::make(dir, ROOT, 0o700)?;
        remove(&path)?;
        let listener = UnixListener::bind(&path)?;
        Ok(Self { listener, path })
    }

    /// Take the worker's link, waiting at most `limit` for it, and stop
    /// listening whatever happens: there is one link per build.
    pub fn accept(self, limit: Duration) -> Result<Link, String> {
        let taken = self.take(limit);
        let _ = remove(&self.path);
        let link = taken?;
        greet(&link.stream).map_err(|e| format!("the link's hello failed: {e}"))?;
        Ok(link)
    }

    /// The first connection, from root, before it said hello.
    fn take(&self, limit: Duration) -> Result<Link, String> {
        set_accept_timeout(&self.listener, limit)
            .map_err(|e| format!("setting how long to wait for the link: {e}"))?;
        let stream = match self.listener.accept() {
            Ok((stream, _)) => stream,
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                return Err(format!(
                    "the worker did not open the link within {} seconds",
                    limit.as_secs()
                ));
            }
            Err(e) => return Err(format!("waiting for the link: {e}")),
        };
        let peer = peer(&stream).map_err(|e| format!("asking who opened the link: {e}"))?;
        if peer.uid != ROOT.uid {
            return Err(format!("the link was opened by uid {}, not root", peer.uid));
        }
        Ok(Link {
            stream,
            pid: peer.pid,
        })
    }
}

/// Say hello, and check the worker's.
fn greet(stream: &UnixStream) -> io::Result<()> {
    let mut writer = stream;
    writer.write_all(HELLO)?;
    stream.set_read_timeout(Some(HELLO_TIME))?;
    let mut theirs = [0; HELLO.len()];
    let mut reader = stream;
    reader.read_exact(&mut theirs)?;
    if &theirs == HELLO {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "the worker said {:?}, where this agent says {:?}",
            String::from_utf8_lossy(&theirs),
            String::from_utf8_lossy(HELLO)
        )))
    }
}

/// The build user's agent socket, served over the link.
pub struct Relay {
    /// The link, kept to shut it: a connection may hold its lock meanwhile.
    link: UnixStream,
    socket: PathBuf,
}

impl Relay {
    /// Serve the build user's socket over `link`, with the hosts' keys that
    /// the build's git trusts beside it: a socket that the build user owns,
    /// mode 0600, in a directory of root's that the build group alone may
    /// enter.
    pub fn start(link: Link, scratch: &Scratch, known_hosts: &str) -> io::Result<Self> {
        let shared_dir = Owner {
            uid: ROOT.uid,
            gid: BUILD.gid,
        };
        dirs::make(&scratch.ssh_dir(), shared_dir, 0o750)?;
        aurcache_common::fs::write_public(&scratch.known_hosts(), known_hosts.as_bytes())?;
        let socket = scratch.ssh_socket();
        remove(&socket)?;
        let listener = UnixListener::bind(&socket)?;
        std::os::unix::fs::lchown(&socket, Some(BUILD.uid), Some(BUILD.gid))?;
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
        Self::over(link.stream, listener, socket)
    }

    /// Serve `listener`, the socket at `socket`, over a link that said hello.
    fn over(link: UnixStream, listener: UnixListener, socket: PathBuf) -> io::Result<Self> {
        link.set_read_timeout(Some(ANSWER_TIME))?;
        link.set_write_timeout(Some(ANSWER_TIME))?;
        let shared = Arc::new(Mutex::new(link.try_clone()?));
        std::thread::spawn(move || serve(&listener, &shared));
        Ok(Self { link, socket })
    }

    /// End it: the socket removed and the link shut, so that a connection
    /// held afterwards reaches nothing. The worker sees the link end.
    pub fn close(self) {
        let _ = remove(&self.socket);
        let _ = self.link.shutdown(Shutdown::Both);
    }
}

/// Take each connection to the build user's socket, and serve it on a thread
/// of its own.
fn serve(listener: &UnixListener, link: &Arc<Mutex<UnixStream>>) {
    let open = Arc::new(AtomicUsize::new(0));
    for stream in listener.incoming() {
        let stream = match stream {
            Ok(stream) => stream,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(_) => return,
        };
        if open.fetch_add(1, Ordering::Relaxed) >= CONNECTIONS_MAX {
            open.fetch_sub(1, Ordering::Relaxed);
            continue;
        }
        let (link, open) = (Arc::clone(link), Arc::clone(&open));
        std::thread::spawn(move || {
            answer(stream, &link);
            open.fetch_sub(1, Ordering::Relaxed);
        });
    }
}

/// One connection of the build's: each request goes over the link and its
/// answer comes back, until either ends. A link that fails once is shut for
/// good, since the next answer on it could be the one this request missed.
fn answer(mut stream: UnixStream, link: &Mutex<UnixStream>) {
    while let Ok(request) = read_message(&mut stream) {
        let reply = {
            let mut link = link.lock().unwrap_or_else(PoisonError::into_inner);
            match write_message(&mut *link, &request).and_then(|()| read_message(&mut *link)) {
                Ok(reply) => reply,
                Err(_) => {
                    let _ = link.shutdown(Shutdown::Both);
                    return;
                }
            }
        };
        if write_message(&mut stream, &reply).is_err() {
            return;
        }
    }
}

/// One agent message: a length, then that many bytes.
fn read_message(from: &mut impl Read) -> io::Result<Vec<u8>> {
    let mut prefix = [0; 4];
    from.read_exact(&mut prefix)?;
    let mut message = vec![0; message_len(u32::from_be_bytes(prefix))?];
    from.read_exact(&mut message)?;
    Ok(message)
}

fn write_message(to: &mut impl Write, message: &[u8]) -> io::Result<()> {
    let len = u32::try_from(message.len()).map_err(io::Error::other)?;
    let mut framed = len.to_be_bytes().to_vec();
    framed.extend_from_slice(message);
    to.write_all(&framed)
}

/// Remove what is at `path`, if anything is.
fn remove(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// The process at the other end of a connection, as the kernel saw it
/// connect.
struct Peer {
    /// Its number in this PID namespace.
    pid: u32,
    uid: u32,
}

fn peer(stream: &UnixStream) -> io::Result<Peer> {
    let mut credentials = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len =
        libc::socklen_t::try_from(std::mem::size_of::<libc::ucred>()).map_err(io::Error::other)?;
    // SAFETY: `credentials` and `len` are valid for `getsockopt` to write,
    // and `len` says how much room there is.
    let got = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut credentials).cast(),
            &raw mut len,
        )
    };
    if got == 0 {
        Ok(Peer {
            pid: u32::try_from(credentials.pid).unwrap_or(0),
            uid: credentials.uid,
        })
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Make `accept` on `listener` fail with `WouldBlock` once `limit` passed
/// (`SO_RCVTIMEO`, which Linux applies to `accept`).
fn set_accept_timeout(listener: &UnixListener, limit: Duration) -> io::Result<()> {
    let timeout = libc::timeval {
        tv_sec: libc::time_t::try_from(limit.as_secs()).map_err(io::Error::other)?,
        tv_usec: i32::try_from(limit.subsec_micros())
            .map_err(io::Error::other)?
            .into(),
    };
    let len = libc::socklen_t::try_from(std::mem::size_of::<libc::timeval>())
        .map_err(io::Error::other)?;
    // SAFETY: `timeout` is a valid `timeval` of `len` bytes, for the length
    // of the call.
    let set = unsafe {
        libc::setsockopt(
            listener.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_RCVTIMEO,
            (&raw const timeout).cast(),
            len,
        )
    };
    if set == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aurcache_build_agent::ssh_link::MESSAGE_MAX;

    /// The worker's end of a link, in a thread: says hello, then answers
    /// each request with its message reversed until the link ends. How many
    /// it answered.
    fn worker(mut stream: UnixStream) -> std::thread::JoinHandle<usize> {
        std::thread::spawn(move || {
            let mut theirs = [0; HELLO.len()];
            stream.read_exact(&mut theirs).unwrap();
            assert_eq!(&theirs, HELLO);
            stream.write_all(HELLO).unwrap();
            let mut answered = 0;
            while let Ok(mut message) = read_message(&mut stream) {
                message.reverse();
                write_message(&mut stream, &message).unwrap();
                answered += 1;
            }
            answered
        })
    }

    fn ask(stream: &mut UnixStream, message: &[u8]) -> Option<Vec<u8>> {
        write_message(stream, message).ok()?;
        read_message(stream).ok()
    }

    struct Relayed {
        relay: Relay,
        socket: PathBuf,
        worker: std::thread::JoinHandle<usize>,
    }

    /// A relay whose link's worker end is a thread, with the build's socket
    /// in `dir`.
    fn relay(dir: &Path) -> Relayed {
        let (agent, worker_end) = UnixStream::pair().unwrap();
        let worker = worker(worker_end);
        greet(&agent).unwrap();
        let socket = dir.join("agent.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let relay = Relay::over(agent, listener, socket.clone()).unwrap();
        Relayed {
            relay,
            socket,
            worker,
        }
    }

    #[test]
    fn requests_go_over_the_link_and_answers_come_back() {
        let dir = tempfile::tempdir().unwrap();
        let Relayed {
            relay,
            socket,
            worker,
        } = relay(dir.path());
        let mut first = UnixStream::connect(&socket).unwrap();
        let mut second = UnixStream::connect(&socket).unwrap();
        assert_eq!(ask(&mut first, &[11, 1, 2]).unwrap(), [2, 1, 11]);
        assert_eq!(ask(&mut second, &[13, 9]).unwrap(), [9, 13]);
        assert_eq!(ask(&mut first, &[5]).unwrap(), [5]);

        relay.close();
        // What the build still holds reaches nothing, and the socket is gone.
        assert!(ask(&mut first, &[11]).is_none());
        assert!(UnixStream::connect(&socket).is_err());
        assert_eq!(worker.join().unwrap(), 3);
    }

    #[test]
    fn a_request_past_the_limit_ends_its_connection_alone() {
        let dir = tempfile::tempdir().unwrap();
        let Relayed {
            relay,
            socket,
            worker,
        } = relay(dir.path());
        let mut hostile = UnixStream::connect(&socket).unwrap();
        hostile.write_all(&(MESSAGE_MAX + 1).to_be_bytes()).unwrap();
        let mut rest = Vec::new();
        assert_eq!(hostile.read_to_end(&mut rest).unwrap(), 0);
        let mut fine = UnixStream::connect(&socket).unwrap();
        assert_eq!(ask(&mut fine, &[11]).unwrap(), [11]);
        relay.close();
        assert_eq!(worker.join().unwrap(), 1);
    }

    #[test]
    fn a_hello_of_another_version_is_refused() {
        let (agent, mut worker_end) = UnixStream::pair().unwrap();
        let other = std::thread::spawn(move || {
            let mut theirs = [0; HELLO.len()];
            worker_end.read_exact(&mut theirs).unwrap();
            worker_end.write_all(b"aurcache-ssh-link 2\n").unwrap();
        });
        let refused = greet(&agent).unwrap_err();
        assert!(
            refused.to_string().contains("aurcache-ssh-link 2"),
            "{refused}"
        );
        other.join().unwrap();
    }

    fn listening(path: &Path) -> Listening {
        let listener = UnixListener::bind(path).unwrap();
        Listening {
            listener,
            path: path.to_path_buf(),
        }
    }

    #[test]
    fn no_link_within_the_limit_is_an_error_and_the_socket_goes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("link.sock");
        let error = listening(&path)
            .accept(Duration::from_millis(200))
            .err()
            .unwrap();
        assert!(error.contains("did not open the link"), "{error}");
        assert!(!path.exists());
    }

    /// The link is taken from root alone, and its process is known by its
    /// number here, which is what the kills spare.
    #[test]
    fn the_link_is_taken_from_root_alone() {
        let (ours, _theirs) = UnixStream::pair().unwrap();
        let me = peer(&ours).unwrap();
        assert_eq!(me.pid, std::process::id());
        // SAFETY: a plain query.
        assert_eq!(me.uid, unsafe { libc::geteuid() });
        if me.uid == ROOT.uid {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("link.sock");
        let listening = listening(&path);
        let _client = UnixStream::connect(&path).unwrap();
        let error = listening.accept(Duration::from_secs(5)).err().unwrap();
        assert!(error.contains(&format!("uid {}", me.uid)), "{error}");
        assert!(!path.exists());
    }
}
