//! The worker's end of the SSH link: the build's agent requests come in over
//! an exec stream, and are answered for the key here.
//!
//! The frames are `aurcache_build_agent::ssh_link`'s. One link per build,
//! opened once: whoever runs this never opens another for the same build,
//! however this one ended, so nothing in the container can open one after
//! the agent closed it.

use super::agent::{Asked, answer};
use super::key::SshKey;
use aurcache_build_agent::ssh_link::{Frame, HEADER_LEN, HELLO, Header, Kind};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// How a link ended, and what it was used for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Served {
    pub signed: u32,
    pub refused: u32,
    pub end: LinkEnd,
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

impl LinkEnd {
    fn lost(error: &std::io::Error) -> Self {
        Self::Lost {
            message: error.to_string(),
        }
    }
}

/// Serve one link to its end: `from_pod` is what `ssh-link` writes, `to_pod`
/// what it reads.
pub async fn serve<R, W>(key: &SshKey, mut from_pod: R, mut to_pod: W) -> Served
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut served = Served {
        signed: 0,
        refused: 0,
        end: LinkEnd::Closed,
    };
    served.end = exchange(key, &mut from_pod, &mut to_pod, &mut served).await;
    let _ = to_pod.shutdown().await;
    served
}

async fn exchange<R, W>(
    key: &SshKey,
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
        let answered = answer(key, &message);
        match answered.asked {
            Asked::Signed => served.signed += 1,
            Asked::Refused(why) => {
                served.refused += 1;
                tracing::debug!("refused an SSH agent request of a build's: {why}");
            }
            Asked::Identities => {}
        }
        let reply = Frame::Reply {
            id: header.id,
            message: answered.message,
        };
        if let Err(e) = write(to_pod, &reply.encode()).await {
            return LinkEnd::lost(&e);
        }
    }
}

async fn write<W: AsyncWrite + Unpin>(to: &mut W, bytes: &[u8]) -> std::io::Result<()> {
    to.write_all(bytes).await?;
    to.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ssh::key::tests::ed25519_key;
    use crate::ssh::wire::Reader;

    async fn frame<R: AsyncRead + Unpin>(from: &mut R) -> Frame {
        let mut header = [0; HEADER_LEN];
        from.read_exact(&mut header).await.unwrap();
        let header = Header::parse(&header).unwrap();
        let mut message = vec![0; header.len as usize];
        from.read_exact(&mut message).await.unwrap();
        Frame::assemble(header, message)
    }

    #[tokio::test]
    async fn requests_are_answered_under_their_id_until_closed() {
        let key = SshKey::parse(ed25519_key().private.as_bytes()).unwrap();
        let (mut pod, worker) = tokio::io::duplex(64 * 1024);
        let (from_pod, to_pod) = tokio::io::split(worker);
        let serving = async { serve(&key, from_pod, to_pod).await };
        let pod_side = async {
            pod.write_all(HELLO).await.unwrap();
            let mut theirs = [0; HELLO.len()];
            pod.read_exact(&mut theirs).await.unwrap();
            assert_eq!(&theirs, HELLO);
            pod.write_all(
                &Frame::Request {
                    id: 41,
                    message: vec![11],
                }
                .encode(),
            )
            .await
            .unwrap();
            pod.write_all(
                &Frame::Request {
                    id: 42,
                    message: vec![22],
                }
                .encode(),
            )
            .await
            .unwrap();
            let Frame::Reply { id, message } = frame(&mut pod).await else {
                panic!("not a reply");
            };
            assert_eq!(id, 41);
            assert_eq!(Reader::new(&message).byte(), Ok(12));
            assert_eq!(
                frame(&mut pod).await,
                Frame::Reply {
                    id: 42,
                    message: vec![5]
                }
            );
            pod.write_all(&Frame::Close.encode()).await.unwrap();
            pod
        };
        let (served, _pod) = tokio::join!(serving, pod_side);
        assert_eq!(
            served,
            Served {
                signed: 0,
                refused: 1,
                end: LinkEnd::Closed
            }
        );
    }

    #[tokio::test]
    async fn another_hello_is_refused_and_a_cut_stream_is_lost() {
        let key = SshKey::parse(ed25519_key().private.as_bytes()).unwrap();
        let (mut pod, worker) = tokio::io::duplex(1024);
        pod.write_all(b"aurcache-ssh-link 9\n").await.unwrap();
        let (from_pod, to_pod) = tokio::io::split(worker);
        let served = serve(&key, from_pod, to_pod).await;
        assert!(matches!(served.end, LinkEnd::Refused { .. }), "{served:?}");

        let (mut pod, worker) = tokio::io::duplex(1024);
        pod.write_all(HELLO).await.unwrap();
        drop(pod);
        let (from_pod, to_pod) = tokio::io::split(worker);
        let served = serve(&key, from_pod, to_pod).await;
        assert!(matches!(served.end, LinkEnd::Lost { .. }), "{served:?}");
    }
}
