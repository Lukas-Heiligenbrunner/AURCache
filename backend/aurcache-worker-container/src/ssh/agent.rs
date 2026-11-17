//! The two requests of the SSH agent protocol a build is answered, and the
//! refusal of every other one.
//!
//! `draft-miller-ssh-agent`: a client asks for the identities and for a
//! signature by one of them. Adding, removing and locking keys, extensions
//! (OpenSSH's client treats `session-bind@openssh.com` as optional) and
//! anything else are answered `SSH_AGENT_FAILURE`, so a build can change
//! nothing another build is served.

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
            message: Writer::new()
                .byte(IDENTITIES_ANSWER)
                .u32(1)
                .string(key.public())
                .string(key.comment().as_bytes())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ssh::key::tests::{ed25519_key, rsa_key};

    fn key() -> SshKey {
        SshKey::parse(ed25519_key().private.as_bytes()).unwrap()
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
        assert_eq!(reader.string(), Ok(&b"aurcache-test"[..]));
        assert!(reader.is_empty());
    }

    #[test]
    fn a_sign_request_for_the_key_is_signed() {
        let key = key();
        let answer = answer(&key, &sign_request(key.public(), b"session", 0));
        assert_eq!(answer.asked, Asked::Signed);
        let mut reader = Reader::new(&answer.message);
        assert_eq!(reader.byte(), Ok(SIGN_RESPONSE));
        let mut signature = Reader::new(reader.string().unwrap());
        assert_eq!(signature.string(), Ok(&b"ssh-ed25519"[..]));
    }

    #[test]
    fn rsa_follows_the_flags_and_refuses_sha1() {
        let key = SshKey::parse(rsa_key().private.as_bytes()).unwrap();
        let name = |flags| {
            let answer = answer(&key, &sign_request(key.public(), b"session", flags));
            let mut reader = Reader::new(&answer.message);
            reader.byte().unwrap();
            let mut signature = Reader::new(reader.string().unwrap());
            String::from_utf8(signature.string().unwrap().to_vec()).unwrap()
        };
        assert_eq!(name(FLAG_RSA_SHA2_256), "rsa-sha2-256");
        assert_eq!(name(FLAG_RSA_SHA2_512), "rsa-sha2-512");
        assert_eq!(name(FLAG_RSA_SHA2_256 | FLAG_RSA_SHA2_512), "rsa-sha2-256");
        let sha1 = answer(&key, &sign_request(key.public(), b"session", 0));
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
        for request in [
            vec![],
            vec![REQUEST_IDENTITIES, 0],
            vec![9],  // remove all identities
            vec![19], // remove all identities, protocol 2
            vec![22], // lock
            add_identity,
            session_bind,
            sign_request(other.public(), b"session", 0),
            vec![SIGN_REQUEST, 0, 0],
            [sign_request(key.public(), b"session", 0), vec![0]].concat(),
        ] {
            let answer = answer(&key, &request);
            assert_eq!(answer.message, [FAILURE], "{request:?}");
            assert!(matches!(answer.asked, Asked::Refused(_)), "{request:?}");
        }
    }

    /// OpenSSH's own client against these answers: `ssh-add -L` lists the
    /// key, and `ssh-keygen -Y sign` signs through the agent alone, with a
    /// signature `ssh-keygen -Y verify` accepts, for each key type.
    #[test]
    fn openssh_signs_through_it() {
        use crate::ssh::key::tests::{Generated, ecdsa_key};
        use std::io::{Read, Write};
        use std::os::unix::net::UnixListener;
        use std::process::Command;
        for made in [ed25519_key(), rsa_key(), ecdsa_key()] {
            let Generated {
                private, public, ..
            } = made;
            let key = std::sync::Arc::new(SshKey::parse(private.as_bytes()).unwrap());
            let dir = tempfile::tempdir().unwrap();
            let socket = dir.path().join("agent.sock");
            let listener = UnixListener::bind(&socket).unwrap();
            let serving = std::sync::Arc::clone(&key);
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
            let listed = Command::new("ssh-add")
                .arg("-L")
                .env("SSH_AUTH_SOCK", &socket)
                .output()
                .expect("ssh-add is needed for this test");
            let listed = String::from_utf8(listed.stdout).unwrap();
            assert_eq!(listed.trim(), public.trim(), "{}", key.kind());

            let public_file = dir.path().join("key.pub");
            std::fs::write(&public_file, &public).unwrap();
            let data = dir.path().join("data");
            std::fs::write(&data, b"signed through the link").unwrap();
            let signed = Command::new("ssh-keygen")
                .args(["-Y", "sign", "-U", "-n", "aurcache", "-f"])
                .arg(&public_file)
                .arg(&data)
                .env("SSH_AUTH_SOCK", &socket)
                .output()
                .unwrap();
            assert!(
                signed.status.success(),
                "{}: {}",
                key.kind(),
                String::from_utf8_lossy(&signed.stderr)
            );
            let signers = dir.path().join("allowed");
            std::fs::write(&signers, format!("test@aurcache {public}")).unwrap();
            let verified = Command::new("ssh-keygen")
                .args([
                    "-Y",
                    "verify",
                    "-n",
                    "aurcache",
                    "-I",
                    "test@aurcache",
                    "-f",
                ])
                .arg(&signers)
                .arg("-s")
                .arg(data.with_extension("sig"))
                .stdin(std::fs::File::open(&data).unwrap())
                .output()
                .unwrap();
            assert!(
                verified.status.success(),
                "{}: {}",
                key.kind(),
                String::from_utf8_lossy(&verified.stderr)
            );
        }
    }
}
