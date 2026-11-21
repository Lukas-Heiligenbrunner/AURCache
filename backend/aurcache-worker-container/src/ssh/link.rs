//! The worker's end of the SSH link (see "The SSH link" in
//! `design/in-progress/container-workers.md`).
//!
//! Only the two requests that a login makes, "which keys are there" and
//! "sign this", reach ssh-agent, unchanged. Every other kind (adding,
//! removing or locking keys, naming a library to load, extensions) is
//! answered with a failure here: none is part of a login, and each is a way
//! for a build to make the worker's ssh-agent hold or do something.
//!
//! Each request goes to ssh-agent on a connection of its own, which is all
//! the two kinds need: neither leaves anything behind for the next.

use aurcache_build_agent::ssh_link::{HELLO, message_len};
use std::io::{self, ErrorKind};
use std::path::Path;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::UnixStream;

/// `SSH_AGENTC_REQUEST_IDENTITIES`: which keys the agent has.
const REQUEST_IDENTITIES: u8 = 11;
/// `SSH_AGENTC_SIGN_REQUEST`: sign this with that key.
const SIGN_REQUEST: u8 = 13;
/// `SSH_AGENT_FAILURE`: the answer to anything an agent does not do.
const FAILURE: u8 = 5;

/// How long ssh-agent has to answer one request.
const ANSWER_TIME: Duration = Duration::from_secs(30);

/// Serve one link to its end against the ssh-agent at `agent`: `from_pod` is
/// what `ssh-link` writes, `to_pod` what it reads. An error is a hello that
/// this worker does not speak, or a stream that broke.
pub async fn serve<R, W>(agent: &Path, mut from_pod: R, mut to_pod: W) -> io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut theirs = [0; HELLO.len()];
    from_pod.read_exact(&mut theirs).await?;
    if &theirs != HELLO {
        return Err(io::Error::other(format!(
            "the build agent said {:?}, where this worker says {:?}",
            String::from_utf8_lossy(&theirs),
            String::from_utf8_lossy(HELLO)
        )));
    }
    to_pod.write_all(HELLO).await?;
    to_pod.flush().await?;
    while let Some(request) = next_request(&mut from_pod).await? {
        let reply = answer(agent, &request).await;
        write_message(&mut to_pod, &reply).await?;
        to_pod.flush().await?;
    }
    to_pod.shutdown().await
}

/// The next request, or `None` once the stream ended between two.
async fn next_request<R: AsyncRead + Unpin>(from: &mut R) -> io::Result<Option<Vec<u8>>> {
    let mut prefix = [0; 4];
    match from.read_exact(&mut prefix).await {
        Ok(_) => {}
        Err(e) if e.kind() == ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let mut message = vec![0; message_len(u32::from_be_bytes(prefix))?];
    from.read_exact(&mut message).await?;
    Ok(Some(message))
}

/// The answer to one request: ssh-agent's, for the kinds that are passed
/// on, and a failure for every other.
async fn answer(agent: &Path, message: &[u8]) -> Vec<u8> {
    if !matches!(message.first(), Some(&(REQUEST_IDENTITIES | SIGN_REQUEST))) {
        return vec![FAILURE];
    }
    match tokio::time::timeout(ANSWER_TIME, ask(agent, message)).await {
        Ok(Ok(reply)) => reply,
        Ok(Err(e)) => {
            tracing::warn!("ssh-agent did not answer a build's request: {e}");
            vec![FAILURE]
        }
        Err(_) => {
            tracing::warn!("ssh-agent did not answer a build's request in time");
            vec![FAILURE]
        }
    }
}

/// One request to ssh-agent and its answer.
async fn ask(agent: &Path, message: &[u8]) -> io::Result<Vec<u8>> {
    let mut stream = UnixStream::connect(agent).await?;
    write_message(&mut stream, message).await?;
    let mut prefix = [0; 4];
    stream.read_exact(&mut prefix).await?;
    let mut reply = vec![0; message_len(u32::from_be_bytes(prefix))?];
    stream.read_exact(&mut reply).await?;
    Ok(reply)
}

/// One agent message: a length, then that many bytes.
async fn write_message<W: AsyncWrite + Unpin>(to: &mut W, message: &[u8]) -> io::Result<()> {
    let len = u32::try_from(message.len()).map_err(io::Error::other)?;
    to.write_all(&len.to_be_bytes()).await?;
    to.write_all(message).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ssh::agent::Agent;
    use crate::ssh::agent::tests::ed25519_key;
    use tokio::io::DuplexStream;

    /// `SSH_AGENT_SIGN_RESPONSE`: a signature.
    const SIGN_RESPONSE: u8 = 14;
    /// `SSH_AGENTC_REMOVE_ALL_IDENTITIES`.
    const REMOVE_ALL: u8 = 19;
    /// `SSH_AGENT_IDENTITIES_ANSWER`.
    const IDENTITIES_ANSWER: u8 = 12;

    /// The pod's side: hello, then each request in turn, then the end of the
    /// stream. What came back for each request.
    async fn pod(mut pod: DuplexStream, requests: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
        pod.write_all(HELLO).await.unwrap();
        let mut theirs = [0; HELLO.len()];
        pod.read_exact(&mut theirs).await.unwrap();
        assert_eq!(&theirs, HELLO);
        let mut replies = Vec::new();
        for message in requests {
            write_message(&mut pod, &message).await.unwrap();
            replies.push(next_request(&mut pod).await.unwrap().unwrap());
        }
        replies
    }

    /// Serve `requests` against the ssh-agent at `agent`: what the pod got
    /// back for each.
    async fn exchanged(agent: &Path, requests: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
        let (pod_end, worker) = tokio::io::duplex(64 * 1024);
        let (from_pod, to_pod) = tokio::io::split(worker);
        let (served, replies) =
            tokio::join!(serve(agent, from_pod, to_pod), pod(pod_end, requests));
        served.unwrap();
        replies
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
        let blob = first_key(&listed[0]);

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
        let replies = exchanged(agent.socket(), requests).await;
        assert_eq!(replies[0][0], SIGN_RESPONSE);
        assert_eq!(replies[1], [FAILURE]);
        assert_eq!(replies[2], [FAILURE]);
        // The key is still there: the removal never reached ssh-agent.
        assert_eq!(first_key(&replies[3]), blob);

        // An agent that is gone fails the request, and the link goes on.
        let nowhere = Path::new("/nonexistent/agent.sock");
        let replies = exchanged(nowhere, vec![vec![REQUEST_IDENTITIES]; 2]).await;
        assert_eq!(replies, [[FAILURE], [FAILURE]]);
    }

    #[tokio::test]
    async fn another_hello_or_a_cut_stream_is_an_error() {
        let nowhere = Path::new("/nonexistent/agent.sock");
        let (mut pod, worker) = tokio::io::duplex(1024);
        pod.write_all(b"aurcache-ssh-link 9\n").await.unwrap();
        let (from_pod, to_pod) = tokio::io::split(worker);
        let refused = serve(nowhere, from_pod, to_pod).await.unwrap_err();
        assert!(refused.to_string().contains("link 9"), "{refused}");

        let (mut pod, worker) = tokio::io::duplex(1024);
        pod.write_all(HELLO).await.unwrap();
        pod.write_all(&[0, 0, 0, 5, REQUEST_IDENTITIES])
            .await
            .unwrap();
        drop(pod);
        let (from_pod, to_pod) = tokio::io::split(worker);
        assert!(serve(nowhere, from_pod, to_pod).await.is_err());
    }
}
