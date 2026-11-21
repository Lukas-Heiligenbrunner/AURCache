//! The worker's end of the SSH link: the build's agent requests come in over
//! an exec stream, and are passed to the build's ssh-agent here.
//!
//! The frames are `aurcache_build_agent::ssh_link`'s. One link per build,
//! from the container's start until the build agent closes it or the
//! container ends.
//!
//! Each request is one message of the SSH agent protocol. Two kinds are
//! passed on, unchanged, and their answers passed back unchanged: the two a
//! login makes, "which keys are there" and "sign this". ssh-agent signs
//! whatever it is asked to, so a build can have anything signed with the
//! key while it runs, a login or not; what it cannot do is hold the key.
//! Every other kind is answered with a failure here and never reaches
//! ssh-agent: adding, removing or locking keys, naming a library for it to
//! load, extensions. None of them is part of a login, and each is a way for
//! a build to make the worker's ssh-agent hold or do something.
//!
//! Each request goes to ssh-agent on a connection of its own, which is all
//! the two kinds need: neither leaves anything behind for the next.

use aurcache_build_agent::ssh_link::{self, Frame, HEADER_LEN, HELLO, Header, Kind};
use std::path::Path;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::UnixStream;

/// `SSH_AGENTC_REQUEST_IDENTITIES`: which keys the agent has.
const REQUEST_IDENTITIES: u8 = 11;
/// `SSH_AGENTC_SIGN_REQUEST`: sign this with that key.
const SIGN_REQUEST: u8 = 13;
/// `SSH_AGENT_SIGN_RESPONSE`: a signature.
const SIGN_RESPONSE: u8 = 14;
/// `SSH_AGENT_FAILURE`: the answer to anything an agent does not do.
const FAILURE: u8 = 5;

/// How long ssh-agent has to answer one request.
const ANSWER_TIME: Duration = Duration::from_secs(30);

/// How a link ended, and what it was used for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Served {
    /// Signatures ssh-agent made.
    pub signed: u32,
    /// Requests of a kind that is not passed on.
    pub refused: u32,
    /// Requests ssh-agent did not answer, and why the last one was not.
    pub unanswered: Option<Unanswered>,
    pub end: LinkEnd,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unanswered {
    pub requests: u32,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkEnd {
    /// The agent closed it: the build no longer needs the key.
    Closed,
    /// The stream ended without the agent closing it.
    Lost { message: String },
    /// The other end is not an agent this worker speaks with.
    Refused { message: String },
}

impl Served {
    /// A link that was never served.
    #[must_use]
    pub fn lost(message: String) -> Self {
        Self {
            signed: 0,
            refused: 0,
            unanswered: None,
            end: LinkEnd::Lost { message },
        }
    }

    /// What the build log says of the link once it is over.
    #[must_use]
    pub fn line(&self) -> String {
        let mut used = format!(
            "{} signature(s) made, {} request(s) refused",
            self.signed, self.refused
        );
        if let Some(Unanswered { requests, message }) = &self.unanswered {
            used.push_str(&format!(
                ", {requests} request(s) ssh-agent did not answer ({message})"
            ));
        }
        match &self.end {
            LinkEnd::Closed => format!("[worker] the SSH link closed: {used}\n"),
            LinkEnd::Lost { message } => {
                format!("[worker] the SSH link ended without being closed ({message}): {used}\n")
            }
            LinkEnd::Refused { message } => {
                format!("[worker] the SSH link was refused: {message}\n")
            }
        }
    }
}

impl LinkEnd {
    fn lost(error: &std::io::Error) -> Self {
        Self::Lost {
            message: error.to_string(),
        }
    }
}

/// Serve one link to its end against the ssh-agent at `agent`: `from_pod` is
/// what `ssh-link` writes, `to_pod` what it reads.
pub async fn serve<R, W>(agent: &Path, mut from_pod: R, mut to_pod: W) -> Served
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut served = Served {
        signed: 0,
        refused: 0,
        unanswered: None,
        end: LinkEnd::Closed,
    };
    served.end = exchange(agent, &mut from_pod, &mut to_pod, &mut served).await;
    let _ = to_pod.shutdown().await;
    served
}

async fn exchange<R, W>(
    agent: &Path,
    from_pod: &mut R,
    to_pod: &mut W,
    served: &mut Served,
) -> LinkEnd
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut theirs = [0; HELLO.len()];
    if let Err(e) = from_pod.read_exact(&mut theirs).await {
        return LinkEnd::lost(&e);
    }
    if &theirs != HELLO {
        return LinkEnd::Refused {
            message: format!(
                "the build agent said {:?}, where this worker says {:?}",
                String::from_utf8_lossy(&theirs),
                String::from_utf8_lossy(HELLO)
            ),
        };
    }
    if let Err(e) = write(to_pod, HELLO).await {
        return LinkEnd::lost(&e);
    }
    loop {
        let mut header = [0; HEADER_LEN];
        if let Err(e) = from_pod.read_exact(&mut header).await {
            return LinkEnd::lost(&e);
        }
        let header = match Header::parse(&header) {
            Ok(header) => header,
            Err(e) => {
                return LinkEnd::Refused {
                    message: format!("the build agent sent {e}"),
                };
            }
        };
        match header.kind {
            Kind::Close => return LinkEnd::Closed,
            Kind::Reply => {
                return LinkEnd::Refused {
                    message: "the build agent sent a reply".to_string(),
                };
            }
            Kind::Request => {}
        }
        let mut message = vec![0; header.len as usize];
        if let Err(e) = from_pod.read_exact(&mut message).await {
            return LinkEnd::lost(&e);
        }
        let reply = Frame::Reply {
            id: header.id,
            message: answer(agent, &message, served).await,
        };
        if let Err(e) = write(to_pod, &reply.encode()).await {
            return LinkEnd::lost(&e);
        }
    }
}

/// The answer to one request: ssh-agent's, for the kinds that are passed
/// on, and a failure for every other.
async fn answer(agent: &Path, message: &[u8], served: &mut Served) -> Vec<u8> {
    if !matches!(message.first(), Some(&(REQUEST_IDENTITIES | SIGN_REQUEST))) {
        served.refused += 1;
        return vec![FAILURE];
    }
    match tokio::time::timeout(ANSWER_TIME, ask(agent, message)).await {
        Ok(Ok(reply)) => {
            if reply.first() == Some(&SIGN_RESPONSE) {
                served.signed += 1;
            }
            reply
        }
        Ok(Err(e)) => unanswered(served, e.to_string()),
        Err(_) => unanswered(
            served,
            format!("no answer in {} seconds", ANSWER_TIME.as_secs()),
        ),
    }
}

/// Note a request ssh-agent did not answer; the build is told it failed.
fn unanswered(served: &mut Served, message: String) -> Vec<u8> {
    let requests = served.unanswered.as_ref().map_or(0, |u| u.requests) + 1;
    if requests == 1 {
        tracing::warn!("ssh-agent did not answer a build's request: {message}");
    }
    served.unanswered = Some(Unanswered { requests, message });
    vec![FAILURE]
}

/// One request to ssh-agent and its answer, as the agent protocol frames
/// them: a length, then that many bytes.
async fn ask(agent: &Path, message: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut stream = UnixStream::connect(agent).await?;
    let len = u32::try_from(message.len()).map_err(std::io::Error::other)?;
    stream.write_all(&len.to_be_bytes()).await?;
    stream.write_all(message).await?;
    let mut prefix = [0; 4];
    stream.read_exact(&mut prefix).await?;
    let len = ssh_link::message_len(u32::from_be_bytes(prefix)).map_err(std::io::Error::other)?;
    let mut reply = vec![0; len as usize];
    stream.read_exact(&mut reply).await?;
    Ok(reply)
}

async fn write<W: AsyncWrite + Unpin>(to: &mut W, bytes: &[u8]) -> std::io::Result<()> {
    to.write_all(bytes).await?;
    to.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ssh::agent::Agent;
    use crate::ssh::agent::tests::ed25519_key;
    use tokio::io::DuplexStream;

    /// `SSH_AGENTC_REMOVE_ALL_IDENTITIES`.
    const REMOVE_ALL: u8 = 19;
    /// `SSH_AGENT_IDENTITIES_ANSWER`.
    const IDENTITIES_ANSWER: u8 = 12;

    async fn frame<R: AsyncRead + Unpin>(from: &mut R) -> Frame {
        let mut header = [0; HEADER_LEN];
        from.read_exact(&mut header).await.unwrap();
        let header = Header::parse(&header).unwrap();
        let mut message = vec![0; header.len as usize];
        from.read_exact(&mut message).await.unwrap();
        Frame::assemble(header, message)
    }

    /// The pod's side: hello, then each request in turn, then a close. What
    /// came back for each request.
    async fn pod(mut pod: DuplexStream, requests: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
        pod.write_all(HELLO).await.unwrap();
        let mut theirs = [0; HELLO.len()];
        pod.read_exact(&mut theirs).await.unwrap();
        assert_eq!(&theirs, HELLO);
        let mut replies = Vec::new();
        for (id, message) in (40..).zip(requests) {
            pod.write_all(&Frame::Request { id, message }.encode())
                .await
                .unwrap();
            let Frame::Reply {
                id: answered,
                message,
            } = frame(&mut pod).await
            else {
                panic!("not a reply");
            };
            assert_eq!(answered, id);
            replies.push(message);
        }
        pod.write_all(&Frame::Close.encode()).await.unwrap();
        replies
    }

    /// A link served to its end, and what the pod got back for each request.
    struct Exchanged {
        served: Served,
        replies: Vec<Vec<u8>>,
    }

    /// Serve `requests` against the ssh-agent at `agent`.
    async fn exchanged(agent: &Path, requests: Vec<Vec<u8>>) -> Exchanged {
        let (pod_end, worker) = tokio::io::duplex(64 * 1024);
        let (from_pod, to_pod) = tokio::io::split(worker);
        let (served, replies) =
            tokio::join!(serve(agent, from_pod, to_pod), pod(pod_end, requests));
        Exchanged { served, replies }
    }

    /// An SSH string: a length, then the bytes.
    fn string(bytes: &[u8]) -> Vec<u8> {
        let mut out = u32::try_from(bytes.len()).unwrap().to_be_bytes().to_vec();
        out.extend_from_slice(bytes);
        out
    }

    /// The first key in an identities answer, as the agent names it.
    fn first_key(answer: &[u8]) -> Vec<u8> {
        assert_eq!(answer[0], IDENTITIES_ANSWER);
        assert_eq!(answer[1..5], 1u32.to_be_bytes());
        let len = u32::from_be_bytes(answer[5..9].try_into().unwrap()) as usize;
        answer[9..9 + len].to_vec()
    }

    #[tokio::test]
    async fn a_login_is_answered_by_ssh_agent_and_nothing_else_reaches_it() {
        let key = ed25519_key();
        let agent = Agent::start(&key.file).await.unwrap();
        let listed = exchanged(agent.socket(), vec![vec![REQUEST_IDENTITIES]]).await;
        assert_eq!(listed.served.end, LinkEnd::Closed);
        let blob = first_key(&listed.replies[0]);

        // Whatever is to be signed: ssh-agent does not ask what it is.
        let mut sign = vec![SIGN_REQUEST];
        sign.extend(string(&blob));
        sign.extend(string(b"anything at all"));
        sign.extend(0u32.to_be_bytes());
        let requests = vec![
            sign,
            vec![REMOVE_ALL],
            // An extension, by name.
            [vec![27], string(b"session-bind@openssh.com")].concat(),
            vec![REQUEST_IDENTITIES],
        ];
        let Exchanged { served, replies } = exchanged(agent.socket(), requests).await;
        assert_eq!(
            served,
            Served {
                signed: 1,
                refused: 2,
                unanswered: None,
                end: LinkEnd::Closed
            }
        );
        assert_eq!(replies[0][0], SIGN_RESPONSE);
        assert_eq!(replies[1], [FAILURE]);
        assert_eq!(replies[2], [FAILURE]);
        // The key is still there: the removal never reached ssh-agent.
        assert_eq!(first_key(&replies[3]), blob);
        assert_eq!(
            served.line(),
            "[worker] the SSH link closed: 1 signature(s) made, 2 request(s) refused\n"
        );
    }

    #[tokio::test]
    async fn an_agent_that_is_gone_fails_the_request_and_the_link_says_so() {
        let key = ed25519_key();
        let agent = Agent::start(&key.file).await.unwrap();
        let socket = agent.socket().to_path_buf();
        drop(agent);
        let Exchanged { served, replies } =
            exchanged(&socket, vec![vec![REQUEST_IDENTITIES]; 2]).await;
        assert_eq!(replies, [[FAILURE], [FAILURE]]);
        assert_eq!(served.signed, 0);
        assert_eq!(served.unanswered.as_ref().unwrap().requests, 2);
        assert_eq!(served.end, LinkEnd::Closed);
        let line = served.line();
        assert!(
            line.contains("2 request(s) ssh-agent did not answer ("),
            "{line}"
        );
    }

    #[tokio::test]
    async fn another_hello_is_refused_and_a_cut_stream_is_lost() {
        let nowhere = Path::new("/nonexistent/agent.sock");
        let (mut pod, worker) = tokio::io::duplex(1024);
        pod.write_all(b"aurcache-ssh-link 9\n").await.unwrap();
        let (from_pod, to_pod) = tokio::io::split(worker);
        let served = serve(nowhere, from_pod, to_pod).await;
        assert!(matches!(served.end, LinkEnd::Refused { .. }), "{served:?}");

        let (mut pod, worker) = tokio::io::duplex(1024);
        pod.write_all(HELLO).await.unwrap();
        drop(pod);
        let (from_pod, to_pod) = tokio::io::split(worker);
        let served = serve(nowhere, from_pod, to_pod).await;
        assert!(matches!(served.end, LinkEnd::Lost { .. }), "{served:?}");
    }
}
