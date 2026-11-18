//! The pacman helper: the one thing a build can ask root for.
//!
//! Root listens on a unix socket. makepkg, through `PACMAN_AUTH`, runs the
//! agent's `pacman-request` subcommand as the build user, which sends the
//! words makepkg gave it and waits for the answer. Root checks the words
//! (`aurcache_build_agent::pacman_request`), runs a command line of its own
//! for the packages they name, with its own environment and no stdin, and
//! answers with how pacman ended. Whatever else arrives on the socket is
//! refused.

use super::children;
use super::output::Output;
use aurcache_build_agent::layout::PACMAN_SOCKET_VAR;
use aurcache_build_agent::pacman_request::{Install, command, validate};
use serde::{Deserialize, Serialize};
use std::fs::Permissions;
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{Command, ExitCode, Stdio};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

/// The longest request read. makepkg's is a line of package names.
const REQUEST_MAX: u64 = 256 * 1024;

/// How long a client has to send its request.
const REQUEST_TIME: Duration = Duration::from_secs(30);

/// What the client exits with when root refused the request.
const REFUSED: u8 = 126;

/// What the client exits with when it got no answer at all.
const UNANSWERED: u8 = 125;

/// What root says to one request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
enum Answer {
    /// pacman ran, and exited with this code.
    Exited { code: i32 },
    /// pacman ran, and a signal ended it.
    Signaled { signal: i32 },
    /// The request is not one root runs.
    Refused { reason: String },
    /// The request was fine and pacman could not be run.
    Failed { message: String },
}

/// Whether the helper still starts pacman.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Gate {
    Open,
    Closed,
}

/// The listening helper.
pub struct Helper {
    gate: Arc<Mutex<Gate>>,
}

impl Helper {
    /// A helper that listens nowhere yet.
    pub fn new() -> Self {
        Self {
            gate: Arc::new(Mutex::new(Gate::Open)),
        }
    }

    /// Listen on `socket` and serve requests from a thread, one at a time.
    ///
    /// Called before the build user has any process, so the socket is
    /// root's own at a path nobody else has touched. Anyone in the container
    /// may connect to it: what is asked is checked, not who asks.
    pub fn listen(&self, socket: &Path, output: Arc<Output>) -> io::Result<()> {
        let listener = UnixListener::bind(socket)?;
        std::fs::set_permissions(socket, Permissions::from_mode(0o666))?;
        let gate = Arc::clone(&self.gate);
        std::thread::spawn(move || serve(&listener, &output, &gate));
        Ok(())
    }

    /// Start no more pacman. Whatever was started before this returns is
    /// running, and dies with everything else when the build is ended.
    pub fn close(&self) {
        *self.gate.lock().unwrap_or_else(PoisonError::into_inner) = Gate::Closed;
    }
}

/// `PACMAN_AUTH`'s client: send `argv` to root, and exit as pacman did.
pub fn request(argv: &[String]) -> ExitCode {
    match ask(argv) {
        Ok(Answer::Exited { code }) => ExitCode::from(u8::try_from(code).unwrap_or(1)),
        Ok(Answer::Signaled { signal }) => {
            ExitCode::from(u8::try_from(128 + signal).unwrap_or(u8::MAX))
        }
        Ok(Answer::Refused { reason }) => {
            eprintln!("aurcache-build-agent: refused: {reason}");
            ExitCode::from(REFUSED)
        }
        Ok(Answer::Failed { message }) => {
            eprintln!("aurcache-build-agent: {message}");
            ExitCode::from(UNANSWERED)
        }
        Err(e) => {
            eprintln!("aurcache-build-agent: could not ask for a package install: {e}");
            ExitCode::from(UNANSWERED)
        }
    }
}

/// One exchange with the helper.
fn ask(argv: &[String]) -> io::Result<Answer> {
    let socket = std::env::var_os(PACMAN_SOCKET_VAR)
        .ok_or_else(|| io::Error::other(format!("{PACMAN_SOCKET_VAR} is not set")))?;
    let mut stream = UnixStream::connect(socket)?;
    stream.write_all(&serde_json::to_vec(argv)?)?;
    stream.shutdown(Shutdown::Write)?;
    let mut answer = Vec::new();
    stream.read_to_end(&mut answer)?;
    Ok(serde_json::from_slice(&answer)?)
}

/// Answer each connection in turn, for as long as the agent runs.
fn serve(listener: &UnixListener, output: &Output, gate: &Mutex<Gate>) {
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        let answer = match read(&stream) {
            Ok(argv) => decide(&argv, |install| install_as_root(install, output, gate)),
            Err(reason) => Answer::Refused { reason },
        };
        if let Answer::Refused { reason } = &answer {
            output.say(&format!(
                "refused a request to run pacman as root: {reason}"
            ));
        }
        if let Ok(bytes) = serde_json::to_vec(&answer) {
            let _ = stream.write_all(&bytes);
        }
    }
}

/// Read one request: a JSON array of words, ended by the client closing its
/// side, within the size and the time a real one takes.
fn read(stream: &UnixStream) -> Result<Vec<String>, String> {
    stream
        .set_read_timeout(Some(REQUEST_TIME))
        .map_err(|e| e.to_string())?;
    let mut bytes = Vec::new();
    stream
        .take(REQUEST_MAX + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("the request could not be read: {e}"))?;
    if bytes.len() as u64 > REQUEST_MAX {
        return Err(format!("the request is longer than {REQUEST_MAX} bytes"));
    }
    // makepkg's requests come through `pacman-request`, which always writes
    // one. This is something else that found the socket.
    serde_json::from_slice(&bytes).map_err(|_| {
        "the request is not a list of words, so it did not come from makepkg".to_string()
    })
}

/// Run what a request asks for, if it is something root runs.
fn decide(argv: &[String], run: impl FnOnce(&Install) -> Answer) -> Answer {
    match validate(argv) {
        Ok(install) => run(&install),
        Err(refusal) => Answer::Refused {
            reason: refusal.to_string(),
        },
    }
}

/// Install packages, as root, with pacman's output in the build log.
///
/// The gate is held while pacman is started, so that none is started once the
/// gate is closed.
fn install_as_root(install: &Install, output: &Output, gate: &Mutex<Gate>) -> Answer {
    let started = {
        let gate = gate.lock().unwrap_or_else(PoisonError::into_inner);
        if *gate == Gate::Closed {
            return Answer::Failed {
                message: "the build is over".to_string(),
            };
        }
        start(install, output)
    };
    match started {
        Ok(pid) => {
            let status = children::wait(pid);
            match (status.code(), status.signal()) {
                (Some(code), _) => Answer::Exited { code },
                (None, signal) => Answer::Signaled {
                    signal: signal.unwrap_or(0),
                },
            }
        }
        Err(e) => Answer::Failed {
            message: format!("pacman could not be started: {e}"),
        },
    }
}

/// Start root's own command line for an install.
fn start(install: &Install, output: &Output) -> io::Result<u32> {
    let words = command(install);
    let (program, arguments) = words
        .split_first()
        .ok_or_else(|| io::Error::other("no command"))?;
    let mut pacman = Command::new(program);
    pacman
        .args(arguments)
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(output.stdio()?)
        .stderr(output.stdio()?);
    Ok(children::spawn(&mut pacman)?.pid)
}

#[cfg(test)]
mod tests {
    use super::{Answer, REQUEST_MAX, decide, read};
    use aurcache_build_agent::pacman_request::Install;
    use std::io::Write;
    use std::net::Shutdown;
    use std::os::unix::net::UnixStream;

    fn words(line: &[&str]) -> Vec<String> {
        line.iter().map(ToString::to_string).collect()
    }

    /// What a client sent, as the helper reads it.
    fn received(bytes: &[u8]) -> Result<Vec<String>, String> {
        let (mut client, server) = UnixStream::pair().unwrap();
        let bytes = bytes.to_vec();
        let writer = std::thread::spawn(move || {
            let _ = client.write_all(&bytes);
            let _ = client.shutdown(Shutdown::Write);
        });
        let got = read(&server);
        drop(server);
        writer.join().unwrap();
        got
    }

    #[test]
    fn only_a_valid_request_reaches_pacman() {
        let ran = |install: &Install| {
            assert_eq!(install.targets, ["tree"]);
            Answer::Exited { code: 0 }
        };
        let good = words(&["/usr/bin/pacman", "--noconfirm", "-S", "--asdeps", "tree"]);
        assert_eq!(decide(&good, ran), Answer::Exited { code: 0 });

        for bad in [
            words(&[
                "/usr/bin/pacman",
                "-U",
                "/work/pkg/evil-1-1-any.pkg.tar.zst",
            ]),
            words(&[
                "/usr/bin/pacman",
                "--config",
                "/tmp/c",
                "-S",
                "--asdeps",
                "tree",
            ]),
            words(&["/usr/bin/pacman", "-S", "--asdeps", "/tmp/evil.pkg.tar.zst"]),
            words(&["/bin/sh", "-c", "id"]),
            words(&[]),
        ] {
            let answer = decide(&bad, |_| panic!("{bad:?} reached pacman"));
            assert!(matches!(answer, Answer::Refused { .. }), "{bad:?}");
        }
    }

    #[test]
    fn a_request_is_a_list_of_words_and_nothing_else() {
        assert_eq!(
            received(br#"["/usr/bin/pacman","-S","--asdeps","tree"]"#),
            Ok(words(&["/usr/bin/pacman", "-S", "--asdeps", "tree"]))
        );
        for junk in [
            &b""[..],
            b"pacman -S --asdeps tree",
            br#"{"argv":["pacman"]}"#,
            br#"["pacman", 1]"#,
            br#"["pacman"] ["-U"]"#,
            b"[\"pacman\xff\"]",
        ] {
            assert!(received(junk).is_err(), "{junk:?}");
        }
    }

    #[test]
    fn a_request_past_the_size_limit_is_not_read() {
        let mut big = b"[\"".to_vec();
        big.resize(usize::try_from(REQUEST_MAX).unwrap() + 1024, b'a');
        big.extend_from_slice(b"\"]");
        let refused = received(&big).unwrap_err();
        assert!(refused.contains("longer than"), "{refused}");
    }

    /// The two halves agree on what an answer looks like on the socket.
    #[test]
    fn an_answer_reads_back_as_it_was_written() {
        for answer in [
            Answer::Exited { code: 1 },
            Answer::Signaled { signal: 9 },
            Answer::Refused {
                reason: "`-U` is not installing dependencies".to_string(),
            },
            Answer::Failed {
                message: "the build is over".to_string(),
            },
        ] {
            let bytes = serde_json::to_vec(&answer).unwrap();
            assert_eq!(serde_json::from_slice::<Answer>(&bytes).unwrap(), answer);
        }
    }
}
