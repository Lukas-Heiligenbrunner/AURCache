//! The build's end of the SSH link: a socket the build user reaches the
//! worker's SSH key through, which holds no key.
//!
//! The worker execs `ssh-link` into the container once; it connects to
//! [`Listening`], root's own socket, and copies bytes to and from the
//! worker. The agent takes that [`Link`] before anything of the job runs,
//! so nothing in the container can be there first. While makepkg runs, a
//! [`Relay`] serves the build user's agent socket over it: each request read
//! there goes over the link as a frame, and the answer that comes back under
//! its id is written back. Closing the relay ends all of it. See `aurcache_build_agent::ssh_link` for the frames, and
//! `container-workers.md`, "The SSH link".

use super::dirs::{self, BUILD, Owner, ROOT};
use aurcache_build_agent::layout::Scratch;
use aurcache_build_agent::ssh_link::{self, Frame, HEADER_LEN, HELLO, Header, Kind};
use std::collections::HashMap;
use std::io::{self, ErrorKind, Read, Write};
use std::net::Shutdown;
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

/// How long the worker has to answer its side of the hello.
const HELLO_TIME: Duration = Duration::from_secs(30);
/// How long one request may wait for its answer.
const ANSWER_TIME: Duration = Duration::from_secs(60);
/// How often a wait for a connection looks again.
const POLL: Duration = Duration::from_millis(100);
/// The most connections of the build's served at once.
const CONNECTIONS_MAX: usize = 32;
/// How long `ssh-link` has to pass on the closing frame and end.
const CLOSE_TIME: Duration = Duration::from_secs(5);

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

/// How the link was when the relay closed it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkEnd {
    /// Up to the end: the build had the key for as long as it was meant to.
    Intact,
    /// It broke while the build could need it.
    Broken { message: String },
}

impl Listening {
    /// Listen, in a directory only root can enter, before anything of the
    /// job runs.
    pub fn open(scratch: &Scratch) -> io::Result<Self> {
        let dir = scratch.ssh_link_dir();
        if let Some(root) = dir.parent() {
            std::fs::create_dir_all(root)?;
            dirs::make(root, ROOT, 0o755)?;
        }
        dirs::make(&dir, ROOT, 0o700)?;
        let path = scratch.ssh_link();
        remove(&path)?;
        let listener = UnixListener::bind(&path)?;
        listener.set_nonblocking(true)?;
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
        let deadline = Instant::now() + limit;
        loop {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    let peer =
                        peer(&stream).map_err(|e| format!("asking who opened the link: {e}"))?;
                    if peer.uid != ROOT.uid {
                        return Err(format!("the link was opened by uid {}, not root", peer.uid));
                    }
                    stream.set_nonblocking(false).map_err(|e| e.to_string())?;
                    return Ok(Link {
                        stream,
                        pid: peer.pid,
                    });
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => return Err(format!("waiting for the link: {e}")),
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "the worker did not open the link within {} seconds",
                    limit.as_secs()
                ));
            }
            wait_readable(&self.listener, POLL);
        }
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
    stream.set_read_timeout(None)?;
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

/// What the relay's threads share.
struct Shared {
    /// The link's writing side. A frame is written whole under the lock.
    link: Mutex<UnixStream>,
    /// Requests sent and not answered, by id.
    waiting: Mutex<HashMap<u32, SyncSender<Vec<u8>>>>,
    next: AtomicU32,
    state: Mutex<State>,
    /// Every connection of the build's being served, to be shut when the
    /// relay closes.
    connections: Mutex<Connections>,
}

#[derive(Default)]
struct Connections {
    open: HashMap<u64, UnixStream>,
    next: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum State {
    Open,
    Broken { message: String },
    Closed,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Shared {
    fn state(&self) -> State {
        lock(&self.state).clone()
    }

    /// The link broke: say so once, and let every waiting request go.
    fn broke(&self, message: String) {
        {
            let mut state = lock(&self.state);
            if *state == State::Open {
                *state = State::Broken { message };
            }
        }
        lock(&self.waiting).clear();
    }

    fn send(&self, frame: &Frame) -> io::Result<()> {
        lock(&self.link).write_all(&frame.encode())
    }
}

/// The build user's agent socket, served over the link.
pub struct Relay {
    shared: Arc<Shared>,
    socket: PathBuf,
    /// Hears from the thread that reads the link once the link has ended.
    ended: mpsc::Receiver<()>,
}

impl Relay {
    /// Serve the build user's socket over `link`: a socket the build user
    /// owns, mode 0600, in a directory of root's the build group alone may
    /// enter.
    pub fn start(link: Link, scratch: &Scratch) -> io::Result<Self> {
        let shared_dir = Owner {
            uid: ROOT.uid,
            gid: BUILD.gid,
        };
        dirs::make(&scratch.ssh_dir(), shared_dir, 0o750)?;
        let socket = scratch.ssh_socket();
        remove(&socket)?;
        let listener = UnixListener::bind(&socket)?;
        std::os::unix::fs::lchown(&socket, Some(BUILD.uid), Some(BUILD.gid))?;
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        Self::over(link.stream, listener, socket)
    }

    /// Serve `listener`, the socket at `socket`, over a link that said hello.
    fn over(link: UnixStream, listener: UnixListener, socket: PathBuf) -> io::Result<Self> {
        let reader = link.try_clone()?;
        let shared = Arc::new(Shared {
            link: Mutex::new(link),
            waiting: Mutex::new(HashMap::new()),
            next: AtomicU32::new(1),
            state: Mutex::new(State::Open),
            connections: Mutex::new(Connections::default()),
        });
        let answers = Arc::clone(&shared);
        let (done, ended) = mpsc::channel();
        std::thread::spawn(move || {
            read_answers(reader, &answers);
            let _ = done.send(());
        });
        let serving = Arc::clone(&shared);
        std::thread::spawn(move || serve(&listener, &serving));
        Ok(Self {
            shared,
            socket,
            ended,
        })
    }

    /// End it: no more connections, every one there is shut, the worker told,
    /// the link shut. Whoever holds a connection afterwards holds nothing.
    ///
    /// The closing frame is given time to reach the worker: `ssh-link` ends
    /// once it has passed it on, and every process is killed soon after.
    pub fn close(self) -> LinkEnd {
        let before = {
            let mut state = lock(&self.shared.state);
            std::mem::replace(&mut *state, State::Closed)
        };
        let _ = remove(&self.socket);
        for (_, connection) in lock(&self.shared.connections).open.drain() {
            let _ = connection.shutdown(Shutdown::Both);
        }
        lock(&self.shared.waiting).clear();
        let _ = self.shared.send(&Frame::Close);
        let _ = lock(&self.shared.link).shutdown(Shutdown::Write);
        let _ = self.ended.recv_timeout(CLOSE_TIME);
        let _ = lock(&self.shared.link).shutdown(Shutdown::Both);
        match before {
            State::Broken { message } => LinkEnd::Broken { message },
            State::Open | State::Closed => LinkEnd::Intact,
        }
    }
}

/// Take each connection to the build user's socket, and serve it on a thread
/// of its own, until the relay closes.
fn serve(listener: &UnixListener, shared: &Arc<Shared>) {
    loop {
        if shared.state() == State::Closed {
            return;
        }
        match listener.accept() {
            Ok((stream, _)) => {
                if stream.set_nonblocking(false).is_err() {
                    continue;
                }
                let Ok(kept) = stream.try_clone() else {
                    continue;
                };
                let key = {
                    let mut connections = lock(&shared.connections);
                    // Checked under the lock `close` drains: a connection is
                    // either shut by it or never served.
                    if shared.state() == State::Closed || connections.open.len() >= CONNECTIONS_MAX
                    {
                        continue;
                    }
                    let key = connections.next;
                    connections.next += 1;
                    connections.open.insert(key, kept);
                    key
                };
                let shared = Arc::clone(shared);
                std::thread::spawn(move || {
                    answer(stream, &shared);
                    lock(&shared.connections).open.remove(&key);
                });
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => wait_readable(listener, POLL),
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(_) => return,
        }
    }
}

/// One connection of the build's: each request it sends goes over the link,
/// and its answer comes back, until it hangs up, sends something that is not
/// a request, or the relay closes.
fn answer(mut stream: UnixStream, shared: &Shared) {
    while let Some(request) = read_message(&mut stream) {
        if shared.state() != State::Open {
            return;
        }
        let id = shared.next.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = mpsc::sync_channel(1);
        lock(&shared.waiting).insert(id, sender);
        let sent = shared.send(&Frame::Request {
            id,
            message: request,
        });
        let reply = sent
            .ok()
            .and_then(|()| receiver.recv_timeout(ANSWER_TIME).ok());
        lock(&shared.waiting).remove(&id);
        let Some(reply) = reply else {
            return;
        };
        let Ok(len) = u32::try_from(reply.len()) else {
            return;
        };
        if stream.write_all(&len.to_be_bytes()).is_err() || stream.write_all(&reply).is_err() {
            return;
        }
    }
}

/// One agent message as the build sends it: a length, then that many
/// bytes. `None` at its end, or for anything else.
fn read_message(stream: &mut UnixStream) -> Option<Vec<u8>> {
    let mut prefix = [0; 4];
    stream.read_exact(&mut prefix).ok()?;
    let len = ssh_link::message_len(u32::from_be_bytes(prefix)).ok()?;
    let mut message = vec![0; len as usize];
    stream.read_exact(&mut message).ok()?;
    Some(message)
}

/// Hand each answer from the worker to the request it answers, until the
/// link ends.
fn read_answers(mut link: UnixStream, shared: &Shared) {
    let ended = loop {
        let mut header = [0; HEADER_LEN];
        if let Err(e) = link.read_exact(&mut header) {
            break format!("the link ended: {e}");
        }
        let header = match Header::parse(&header) {
            Ok(header) => header,
            Err(e) => break format!("the worker sent {e}"),
        };
        if header.kind != Kind::Reply {
            break format!("the worker sent a frame of kind {:?}", header.kind);
        }
        let mut message = vec![0; header.len as usize];
        if let Err(e) = link.read_exact(&mut message) {
            break format!("the link ended: {e}");
        }
        if let Some(waiting) = lock(&shared.waiting).remove(&header.id) {
            let _ = waiting.send(message);
        }
    };
    shared.broke(ended);
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

/// Who is at the other end of a connection.
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

/// Wait until `listener` has a connection to take, or `limit` passed.
fn wait_readable(listener: &UnixListener, limit: Duration) {
    let mut poll = libc::pollfd {
        fd: listener.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let millis = libc::c_int::try_from(limit.as_millis()).unwrap_or(libc::c_int::MAX);
    // SAFETY: one valid `pollfd`, for the length of the call.
    unsafe { libc::poll(&raw mut poll, 1, millis) };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The worker's end of a link, in a thread: says hello, then answers
    /// each request with its message reversed, until the agent closes, and
    /// hangs up as `ssh-link` does.
    fn worker(mut stream: UnixStream) -> std::thread::JoinHandle<Vec<Frame>> {
        std::thread::spawn(move || {
            let mut seen = Vec::new();
            let mut theirs = [0; HELLO.len()];
            stream.read_exact(&mut theirs).unwrap();
            assert_eq!(&theirs, HELLO);
            stream.write_all(HELLO).unwrap();
            loop {
                let mut header = [0; HEADER_LEN];
                if stream.read_exact(&mut header).is_err() {
                    return seen;
                }
                let header = Header::parse(&header).unwrap();
                let mut message = vec![0; header.len as usize];
                stream.read_exact(&mut message).unwrap();
                let frame = Frame::assemble(header, message);
                if let Frame::Request { id, message } = &frame {
                    let mut reversed = message.clone();
                    reversed.reverse();
                    let reply = Frame::Reply {
                        id: *id,
                        message: reversed,
                    };
                    stream.write_all(&reply.encode()).unwrap();
                }
                let closed = frame == Frame::Close;
                seen.push(frame);
                if closed {
                    return seen;
                }
            }
        })
    }

    fn ask(stream: &mut UnixStream, message: &[u8]) -> Option<Vec<u8>> {
        let len = u32::try_from(message.len()).unwrap();
        stream.write_all(&len.to_be_bytes()).ok()?;
        stream.write_all(message).ok()?;
        read_message(stream)
    }

    /// A relay under test, the build's socket, and the thread that is the
    /// worker's end, which hands back every frame it saw.
    struct Relayed {
        relay: Relay,
        socket: PathBuf,
        worker: std::thread::JoinHandle<Vec<Frame>>,
    }

    /// A relay over a link whose worker end is in a thread, with the build's
    /// socket in a directory of the test's own.
    fn relay(dir: &Path) -> Relayed {
        let (agent, worker_end) = UnixStream::pair().unwrap();
        let worker = worker(worker_end);
        greet(&agent).unwrap();
        let socket = dir.join("agent.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
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

        assert_eq!(relay.close(), LinkEnd::Intact);
        // Everything the build held is shut, and the socket is gone.
        assert!(ask(&mut first, &[11]).is_none());
        assert!(ask(&mut second, &[11]).is_none());
        assert!(UnixStream::connect(&socket).is_err());
        let seen = worker.join().unwrap();
        assert_eq!(seen.last(), Some(&Frame::Close));
        assert_eq!(seen.len(), 4);
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
        hostile
            .write_all(&(ssh_link::MESSAGE_MAX + 1).to_be_bytes())
            .unwrap();
        let mut rest = Vec::new();
        assert_eq!(hostile.read_to_end(&mut rest).unwrap(), 0);
        let mut fine = UnixStream::connect(&socket).unwrap();
        assert_eq!(ask(&mut fine, &[11]).unwrap(), [11]);
        assert_eq!(relay.close(), LinkEnd::Intact);
        assert_eq!(worker.join().unwrap().len(), 2);
    }

    #[test]
    fn a_link_that_broke_is_said_to_have() {
        let dir = tempfile::tempdir().unwrap();
        let (agent, mut worker_end) = UnixStream::pair().unwrap();
        let hello = std::thread::spawn(move || {
            let mut theirs = [0; HELLO.len()];
            worker_end.read_exact(&mut theirs).unwrap();
            worker_end.write_all(HELLO).unwrap();
            worker_end
        });
        greet(&agent).unwrap();
        let worker_end = hello.join().unwrap();
        let scratch_socket = dir.path().join("agent.sock");
        let listener = UnixListener::bind(&scratch_socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let relay = Relay::over(agent, listener, scratch_socket.clone()).unwrap();
        drop(worker_end);
        // Once the reading thread has seen the link end.
        let deadline = Instant::now() + Duration::from_secs(10);
        while relay.shared.state() == State::Open && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let mut client = UnixStream::connect(&scratch_socket).unwrap();
        assert!(ask(&mut client, &[11]).is_none());
        assert!(matches!(relay.close(), LinkEnd::Broken { .. }));
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

    #[test]
    fn no_link_within_the_limit_is_an_error_and_the_socket_goes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("link.sock");
        let listener = UnixListener::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let listening = Listening {
            listener,
            path: path.clone(),
        };
        let error = listening.accept(Duration::from_millis(200)).err().unwrap();
        assert!(error.contains("did not open the link"), "{error}");
        assert!(!path.exists());
    }

    /// The process at the other end is known by its number here, which is
    /// what the kills spare.
    #[test]
    fn a_peer_is_known_by_its_number() {
        let (ours, _theirs) = UnixStream::pair().unwrap();
        let peer = peer(&ours).unwrap();
        assert_eq!(peer.pid, std::process::id());
        // SAFETY: a plain query.
        assert_eq!(peer.uid, unsafe { libc::geteuid() });
    }

    #[test]
    fn the_link_is_taken_from_root_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("link.sock");
        let listener = UnixListener::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let listening = Listening {
            listener,
            path: path.clone(),
        };
        // SAFETY: a plain query.
        let me = unsafe { libc::geteuid() };
        if me == 0 {
            // Root's connection is the one taken; nothing to refuse here.
            return;
        }
        let _client = UnixStream::connect(&path).unwrap();
        let error = listening.accept(Duration::from_secs(5)).err().unwrap();
        assert!(error.contains(&format!("uid {me}")), "{error}");
        assert!(!path.exists());
    }
}
