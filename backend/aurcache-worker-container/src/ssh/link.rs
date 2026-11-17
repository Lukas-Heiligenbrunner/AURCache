//! The worker's end of the SSH link: the build's agent requests come in over
//! an exec stream, and are answered for the key here.
//!
//! The frames are `aurcache_build_agent::ssh_link`'s. One link per build,
//! opened once: whoever runs this never opens another for the same build,
//! however this one ended, so nothing in the container can open one after
//! the agent closed it.

use super::agent::{Answer, Asked, answer, is_sign_request, refusal};
use super::key::SshKey;
use aurcache_build_agent::ssh_link::{Frame, HEADER_LEN, HELLO, Header, Kind};
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::time::Instant;

/// The most signatures one link is given. A git fetch logs in once per
/// remote, so a package's download needs a handful; this leaves room for
/// many sources and retries, and bounds what a build can take.
pub const SIGNATURES_MAX: u32 = 64;
/// The most signatures in any [`RATE_WINDOW`].
pub const RATE_MAX: usize = 16;
pub const RATE_WINDOW: Duration = Duration::from_secs(60);

/// How many signatures a link has been given, and when.
struct Budget {
    total: u32,
    recent: VecDeque<Instant>,
}

impl Budget {
    fn new() -> Self {
        Self {
            total: 0,
            recent: VecDeque::new(),
        }
    }

    /// Whether one more signature is allowed now; counted if it is.
    fn take(&mut self, now: Instant) -> Spend {
        while self
            .recent
            .front()
            .is_some_and(|at| now.duration_since(*at) >= RATE_WINDOW)
        {
            self.recent.pop_front();
        }
        if self.total >= SIGNATURES_MAX {
            return Spend::Exhausted;
        }
        if self.recent.len() >= RATE_MAX {
            return Spend::TooFast;
        }
        self.total += 1;
        self.recent.push_back(now);
        Spend::Granted
    }
}

/// What the budget says to one more signature.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Spend {
    Granted,
    Exhausted,
    TooFast,
}

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

impl Served {
    /// What the build log says of the link once it is over.
    #[must_use]
    pub fn line(&self) -> String {
        let used = format!(
            "{} signature(s) made, {} request(s) refused",
            self.signed, self.refused
        );
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

/// Serve one link to its end: `from_pod` is what `ssh-link` writes, `to_pod`
/// what it reads.
pub async fn serve<R, W>(key: Arc<SshKey>, mut from_pod: R, mut to_pod: W) -> Served
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut served = Served {
        signed: 0,
        refused: 0,
        end: LinkEnd::Closed,
    };
    served.end = exchange(&key, &mut from_pod, &mut to_pod, &mut served).await;
    let _ = to_pod.shutdown().await;
    served
}

async fn exchange<R, W>(
    key: &Arc<SshKey>,
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
    let mut budget = Budget::new();
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
        let answered = if is_sign_request(&message) {
            match budget.take(Instant::now()) {
                Spend::Granted => sign_apart(key, message).await,
                Spend::Exhausted => refusal("past the signatures one link is given"),
                Spend::TooFast => refusal("signatures asked for faster than allowed"),
            }
        } else {
            answer(key, &message)
        };
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

/// Answer a sign request on a blocking thread: an RSA signature takes
/// milliseconds, which the runtime's own threads are not for.
async fn sign_apart(key: &Arc<SshKey>, message: Vec<u8>) -> Answer {
    let key = Arc::clone(key);
    tokio::task::spawn_blocking(move || answer(&key, &message))
        .await
        .unwrap_or_else(|_| refusal("a signature that failed"))
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
        let serving = async { serve(Arc::new(key), from_pod, to_pod).await };
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
        let key = Arc::new(SshKey::parse(ed25519_key().private.as_bytes()).unwrap());
        let (mut pod, worker) = tokio::io::duplex(1024);
        pod.write_all(b"aurcache-ssh-link 9\n").await.unwrap();
        let (from_pod, to_pod) = tokio::io::split(worker);
        let served = serve(Arc::clone(&key), from_pod, to_pod).await;
        assert!(matches!(served.end, LinkEnd::Refused { .. }), "{served:?}");

        let (mut pod, worker) = tokio::io::duplex(1024);
        pod.write_all(HELLO).await.unwrap();
        drop(pod);
        let (from_pod, to_pod) = tokio::io::split(worker);
        let served = serve(Arc::clone(&key), from_pod, to_pod).await;
        assert!(matches!(served.end, LinkEnd::Lost { .. }), "{served:?}");
    }

    #[test]
    fn the_budget_holds_a_rate_and_a_total() {
        let start = Instant::now();
        let mut budget = Budget::new();
        for _ in 0..RATE_MAX {
            assert_eq!(budget.take(start), Spend::Granted);
        }
        assert_eq!(budget.take(start), Spend::TooFast);
        // A window later, the rate allows more, up to the total.
        let mut at = start;
        let mut granted = u32::try_from(RATE_MAX).unwrap();
        while granted < SIGNATURES_MAX {
            at += RATE_WINDOW;
            for _ in 0..RATE_MAX {
                if budget.take(at) == Spend::Granted {
                    granted += 1;
                }
            }
        }
        at += RATE_WINDOW;
        assert_eq!(budget.take(at), Spend::Exhausted);
    }
}
