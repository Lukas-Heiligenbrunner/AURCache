//! The two requests of the SSH agent protocol a build is answered, and the
//! refusal of every other one.
//!
//! `draft-miller-ssh-agent`: a client asks for the identities and for a
//! signature by one of them, and the signature is made only over what an SSH
//! client signs to log in with this key. Adding, removing and locking keys,
//! extensions (OpenSSH's client treats `session-bind@openssh.com` as
//! optional), signatures over anything else and every other request are
//! answered `SSH_AGENT_FAILURE`, so a build can change nothing another build
//! is served, and can use the key to log in and for nothing else.

use super::key::{RsaHash, SshKey, Unsigned};
use super::wire::{Reader, Writer};

const FAILURE: u8 = 5;
const REQUEST_IDENTITIES: u8 = 11;
const IDENTITIES_ANSWER: u8 = 12;
const SIGN_REQUEST: u8 = 13;
const SIGN_RESPONSE: u8 = 14;

const FLAG_RSA_SHA2_256: u32 = 2;
const FLAG_RSA_SHA2_512: u32 = 4;

/// What a request was, for the worker's account of the link.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Asked {
    Identities,
    Signed,
    /// Refused, and why, in a few words.
    Refused(&'static str),
}

/// The answer to one request, and what it was.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Answer {
    pub message: Vec<u8>,
    pub asked: Asked,
}

/// A refusal, for a request the caller will not pass on.
#[must_use]
pub fn refusal(why: &'static str) -> Answer {
    refused(why)
}

/// Whether a message asks for a signature.
#[must_use]
pub fn is_sign_request(message: &[u8]) -> bool {
    message.first() == Some(&SIGN_REQUEST)
}

fn refused(why: &'static str) -> Answer {
    Answer {
        message: vec![FAILURE],
        asked: Asked::Refused(why),
    }
}

/// Answer one agent message (without its length prefix) for `key`.
#[must_use]
pub fn answer(key: &SshKey, request: &[u8]) -> Answer {
    let mut reader = Reader::new(request);
    match reader.byte() {
        Ok(REQUEST_IDENTITIES) if reader.is_empty() => Answer {
            // No comment: the build has no use for one, and a key's comment
            // can carry its origin.
            message: Writer::new()
                .byte(IDENTITIES_ANSWER)
                .u32(1)
                .string(key.public())
                .string(b"")
                .finish(),
            asked: Asked::Identities,
        },
        Ok(SIGN_REQUEST) => sign(key, &mut reader),
        Ok(_) => refused("not a request this agent answers"),
        Err(_) => refused("an empty message"),
    }
}

fn sign(key: &SshKey, reader: &mut Reader<'_>) -> Answer {
    let (Ok(blob), Ok(data), Ok(flags)) = (reader.string(), reader.string(), reader.u32()) else {
        return refused("a malformed sign request");
    };
    if !reader.is_empty() {
        return refused("a malformed sign request");
    }
    if blob != key.public() {
        return refused("a sign request for another key");
    }
    if !is_userauth(data, key.public()) {
        return refused("data that is not an SSH login with this key");
    }
    // OpenSSH's own agent's order, when a client sets both.
    let rsa = if flags & FLAG_RSA_SHA2_256 != 0 {
        Some(RsaHash::Sha256)
    } else if flags & FLAG_RSA_SHA2_512 != 0 {
        Some(RsaHash::Sha512)
    } else {
        None
    };
    match key.sign(data, rsa) {
        Ok(signature) => Answer {
            message: Writer::new()
                .byte(SIGN_RESPONSE)
                .string(&signature)
                .finish(),
            asked: Asked::Signed,
        },
        Err(Unsigned::RsaSha1) => refused("an RSA signature with SHA-1"),
        Err(Unsigned::Failed) => refused("a signature that failed"),
    }
}

/// `SSH_MSG_USERAUTH_REQUEST`.
const USERAUTH_REQUEST: u8 = 50;
const PUBLICKEY: &[u8] = b"publickey";
const PUBLICKEY_HOSTBOUND: &[u8] = b"publickey-hostbound-v00@openssh.com";

/// Whether `data` is what an SSH client signs to log in with `public`
/// (RFC 4252, section 7; OpenSSH's `publickey-hostbound-v00` adds the
/// server's host key): the session id, `SSH_MSG_USERAUTH_REQUEST`, a user,
/// `ssh-connection`, the method, TRUE, an algorithm, and this key.
///
/// So the key signs logins and nothing else: no SSHSIG, no git commit or tag
/// signature, no blob of the build's choosing.
fn is_userauth(data: &[u8], public: &[u8]) -> bool {
    let mut reader = Reader::new(data);
    let parsed = (|| {
        reader.string()?; // the session id
        if reader.byte()? != USERAUTH_REQUEST {
            return Ok(false);
        }
        reader.string()?; // the user
        if reader.string()? != b"ssh-connection" {
            return Ok(false);
        }
        let method = reader.string()?;
        if method != PUBLICKEY && method != PUBLICKEY_HOSTBOUND {
            return Ok(false);
        }
        if reader.byte()? != 1 {
            return Ok(false);
        }
        reader.string()?; // the algorithm
        if reader.string()? != public {
            return Ok(false);
        }
        if method == PUBLICKEY_HOSTBOUND {
            reader.string()?; // the server's host key
        }
        Ok::<bool, super::wire::Truncated>(reader.is_empty())
    })();
    parsed.unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ssh::key::tests::{ed25519_key, rsa_key};

    fn key() -> SshKey {
        SshKey::parse(ed25519_key().private.as_bytes()).unwrap()
    }

    /// What a client signs to log in with `public`, by `method`.
    fn userauth(public: &[u8], method: &[u8]) -> Vec<u8> {
        let mut writer = Writer::new();
        writer
            .string(b"session id")
            .byte(USERAUTH_REQUEST)
            .string(b"git")
            .string(b"ssh-connection")
            .string(method)
            .byte(1)
            .string(b"ssh-ed25519")
            .string(public);
        if method == PUBLICKEY_HOSTBOUND {
            writer.string(b"host key");
        }
        writer.finish()
    }

    fn sign_request(blob: &[u8], data: &[u8], flags: u32) -> Vec<u8> {
        Writer::new()
            .byte(SIGN_REQUEST)
            .string(blob)
            .string(data)
            .u32(flags)
            .finish()
    }

    #[test]
    fn the_identities_are_the_one_key() {
        let key = key();
        let answer = answer(&key, &[REQUEST_IDENTITIES]);
        assert_eq!(answer.asked, Asked::Identities);
        let mut reader = Reader::new(&answer.message);
        assert_eq!(reader.byte(), Ok(IDENTITIES_ANSWER));
        assert_eq!(reader.u32(), Ok(1));
        assert_eq!(reader.string(), Ok(key.public()));
        assert_eq!(reader.string(), Ok(&b""[..]));
        assert!(reader.is_empty());
    }

    #[test]
    fn a_sign_request_for_the_key_is_signed() {
        let key = key();
        for method in [PUBLICKEY, PUBLICKEY_HOSTBOUND] {
            let login = userauth(key.public(), method);
            let answer = answer(&key, &sign_request(key.public(), &login, 0));
            assert_eq!(answer.asked, Asked::Signed);
            let mut reader = Reader::new(&answer.message);
            assert_eq!(reader.byte(), Ok(SIGN_RESPONSE));
            let mut signature = Reader::new(reader.string().unwrap());
            assert_eq!(signature.string(), Ok(&b"ssh-ed25519"[..]));
        }
    }

    #[test]
    fn rsa_follows_the_flags_and_refuses_sha1() {
        let key = SshKey::parse(rsa_key().private.as_bytes()).unwrap();
        let login = userauth(key.public(), PUBLICKEY);
        let name = |flags| {
            let answer = answer(&key, &sign_request(key.public(), &login, flags));
            let mut reader = Reader::new(&answer.message);
            reader.byte().unwrap();
            let mut signature = Reader::new(reader.string().unwrap());
            String::from_utf8(signature.string().unwrap().to_vec()).unwrap()
        };
        assert_eq!(name(FLAG_RSA_SHA2_256), "rsa-sha2-256");
        assert_eq!(name(FLAG_RSA_SHA2_512), "rsa-sha2-512");
        assert_eq!(name(FLAG_RSA_SHA2_256 | FLAG_RSA_SHA2_512), "rsa-sha2-256");
        let sha1 = answer(&key, &sign_request(key.public(), &login, 0));
        assert_eq!(sha1.message, [FAILURE]);
    }

    #[test]
    fn everything_else_is_refused() {
        let key = key();
        let other = SshKey::parse(ed25519_key().private.as_bytes()).unwrap();
        let session_bind = Writer::new()
            .byte(27)
            .string(b"session-bind@openssh.com")
            .finish();
        let add_identity = Writer::new().byte(17).string(b"ssh-ed25519").finish();
        let login = userauth(key.public(), PUBLICKEY);
        // A login whose method, service, key, flag or length is not the
        // one a client signs.
        let mut not_logins = vec![
            b"session".to_vec(),
            b"SSHSIG\0\0\0\x03git".to_vec(),
            userauth(key.public(), b"password"),
            userauth(other.public(), PUBLICKEY),
            [login.clone(), vec![0]].concat(),
            login[..login.len() - 1].to_vec(),
        ];
        let mut service = login.clone();
        let at = service
            .windows(b"ssh-connection".len())
            .position(|w| w == b"ssh-connection")
            .unwrap();
        service[at] = b'x';
        not_logins.push(service);
        let mut flag = login.clone();
        let at = flag
            .windows(PUBLICKEY.len())
            .position(|w| w == PUBLICKEY)
            .unwrap()
            + PUBLICKEY.len();
        flag[at] = 0;
        not_logins.push(flag);
        for data in &not_logins {
            let answer = answer(&key, &sign_request(key.public(), data, 0));
            assert_eq!(answer.message, [FAILURE], "{data:?}");
        }
        for request in [
            vec![],
            vec![REQUEST_IDENTITIES, 0],
            vec![9],  // remove all identities
            vec![19], // remove all identities, protocol 2
            vec![22], // lock
            add_identity,
            session_bind,
            sign_request(other.public(), &login, 0),
            vec![SIGN_REQUEST, 0, 0],
            [sign_request(key.public(), &login, 0), vec![0]].concat(),
        ] {
            let answer = answer(&key, &request);
            assert_eq!(answer.message, [FAILURE], "{request:?}");
            assert!(matches!(answer.asked, Asked::Refused(_)), "{request:?}");
        }
    }

    /// OpenSSH's own client against these answers: `ssh-add -L` lists the
    /// key; `ssh-keygen -Y sign`, an SSHSIG and not a login, is refused; and
    /// a real `ssh` login to a throwaway `sshd` succeeds through the agent
    /// alone, for each key type.
    #[test]
    fn openssh_logs_in_through_it_and_signs_nothing_else() {
        use crate::ssh::key::tests::{Generated, ecdsa_key};
        use std::process::Command;
        let sshd = ["/usr/bin/sshd", "/usr/sbin/sshd"]
            .into_iter()
            .find(|path| std::path::Path::new(path).exists());
        for made in [ed25519_key(), rsa_key(), ecdsa_key()] {
            let Generated {
                private, public, ..
            } = made;
            let key = std::sync::Arc::new(SshKey::parse(private.as_bytes()).unwrap());
            let dir = tempfile::tempdir().unwrap();
            let socket = serve(&dir.path().join("agent.sock"), &key);
            let listed = Command::new("ssh-add")
                .arg("-L")
                .env("SSH_AUTH_SOCK", &socket)
                .output()
                .expect("ssh-add is needed for this test");
            let listed = String::from_utf8(listed.stdout).unwrap();
            let expected = public
                .split_whitespace()
                .take(2)
                .collect::<Vec<_>>()
                .join(" ");
            assert_eq!(listed.trim(), expected, "{}", key.kind());

            let public_file = dir.path().join("key.pub");
            std::fs::write(&public_file, &public).unwrap();
            let data = dir.path().join("data");
            std::fs::write(&data, b"not a login").unwrap();
            let signed = Command::new("ssh-keygen")
                .args(["-Y", "sign", "-U", "-n", "aurcache", "-f"])
                .arg(&public_file)
                .arg(&data)
                .env("SSH_AUTH_SOCK", &socket)
                .output()
                .unwrap();
            assert!(
                !signed.status.success(),
                "{}: an SSHSIG was made",
                key.kind()
            );

            let Some(sshd) = sshd else {
                eprintln!("sshd is not installed: the login half of this test is not run");
                continue;
            };
            login(sshd, dir.path(), &public, &socket, key.kind());
        }
    }

    /// Serve `key` on a socket at `path`, one thread per connection.
    fn serve(path: &std::path::Path, key: &std::sync::Arc<SshKey>) -> std::path::PathBuf {
        use std::io::{Read, Write};
        use std::os::unix::net::UnixListener;
        let listener = UnixListener::bind(path).unwrap();
        let serving = std::sync::Arc::clone(key);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let key = std::sync::Arc::clone(&serving);
                std::thread::spawn(move || {
                    loop {
                        let mut len = [0; 4];
                        if stream.read_exact(&mut len).is_err() {
                            return;
                        }
                        let mut request = vec![0; u32::from_be_bytes(len) as usize];
                        stream.read_exact(&mut request).unwrap();
                        let reply = answer(&key, &request).message;
                        let len = u32::try_from(reply.len()).unwrap().to_be_bytes();
                        stream.write_all(&len).unwrap();
                        stream.write_all(&reply).unwrap();
                    }
                });
            }
        });
        path.to_path_buf()
    }

    /// Log in, as this user, to an `sshd` of this test's own that accepts
    /// `public`, with the agent at `socket` as the only key.
    fn login(
        sshd: &str,
        dir: &std::path::Path,
        public: &str,
        socket: &std::path::Path,
        kind: &str,
    ) {
        use std::process::{Command, Stdio};
        let host_key = dir.join("host_key");
        assert!(
            Command::new("ssh-keygen")
                .args(["-q", "-t", "ed25519", "-N", "", "-f"])
                .arg(&host_key)
                .status()
                .unwrap()
                .success()
        );
        let authorized = dir.join("authorized_keys");
        std::fs::write(&authorized, public).unwrap();
        let port = {
            let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            probe.local_addr().unwrap().port()
        };
        let config = dir.join("sshd_config");
        std::fs::write(
            &config,
            format!(
                "Port {port}\nListenAddress 127.0.0.1\nHostKey {}\nAuthorizedKeysFile {}\n\
                 PidFile none\nUsePAM no\nStrictModes no\nPasswordAuthentication no\n\
                 KbdInteractiveAuthentication no\n",
                host_key.display(),
                authorized.display()
            ),
        )
        .unwrap();
        let mut server = Command::new(sshd)
            .args(["-D", "-e", "-f"])
            .arg(&config)
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let known = dir.join("known_hosts");
        let host_public = std::fs::read_to_string(host_key.with_extension("pub")).unwrap();
        std::fs::write(&known, format!("[127.0.0.1]:{port} {host_public}")).unwrap();
        let user = std::env::var("USER").unwrap_or_else(|_| "root".into());
        let mut logged_in = None;
        for _ in 0..50 {
            let out = Command::new("ssh")
                .args(["-F", "/dev/null", "-p", &port.to_string()])
                .arg("-o")
                .arg(format!("UserKnownHostsFile={}", known.display()))
                .args([
                    "-o",
                    "BatchMode=yes",
                    "-o",
                    "IdentitiesOnly=no",
                    "-o",
                    "PreferredAuthentications=publickey",
                ])
                .arg(format!("{user}@127.0.0.1"))
                .arg("echo logged-in")
                .env("SSH_AUTH_SOCK", socket)
                .output()
                .unwrap();
            if String::from_utf8_lossy(&out.stdout).contains("logged-in") {
                logged_in = Some(());
                break;
            }
            logged_in = None;
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        server.kill().ok();
        server.wait().ok();
        assert!(
            logged_in.is_some(),
            "{kind}: ssh did not log in through the agent"
        );
    }
}
