//! The link between a build and the worker that holds its SSH key.
//!
//! The worker execs `aurcache-build-agent ssh-link` into the build container
//! once, and that process copies bytes between its stdin and stdout and a
//! socket the agent listens on. Over it, the agent forwards each request the
//! build sends its SSH agent socket, and the worker sends back the answer
//! of an ssh-agent on its own side. The key stays with the worker. See
//! `container-workers.md`, "The SSH link".
//!
//! Both ends first send [`HELLO`] and check the other's. After that, every
//! frame is a [`HEADER_LEN`]-byte header (kind, id, length, big-endian) and
//! that many bytes:
//!
//! - [`Frame::Request`], agent to worker: one agent protocol message as the
//!   build sent it, without its length prefix;
//! - [`Frame::Reply`], worker to agent: the answer to the request of the same
//!   id;
//! - [`Frame::Close`], agent to worker: the build no longer needs the key;
//!   nothing follows.
//!
//! The worker keeps no state per connection of the build's: the agent
//! protocol is one answer per request, and the id says whose it is.

/// What each end sends first.
pub const HELLO: &[u8; 20] = b"aurcache-ssh-link 1\n";

/// The longest agent message either way, as OpenSSH's own agent limits it.
pub const MESSAGE_MAX: u32 = 256 * 1024;

/// The bytes before a frame's message.
pub const HEADER_LEN: usize = 9;

const REQUEST: u8 = 1;
const REPLY: u8 = 2;
const CLOSE: u8 = 3;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Frame {
    Request { id: u32, message: Vec<u8> },
    Reply { id: u32, message: Vec<u8> },
    Close,
}

/// A frame's kind, before its message has been read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Request,
    Reply,
    Close,
}

/// What a frame's header says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub kind: Kind,
    pub id: u32,
    /// The length of the message that follows.
    pub len: u32,
}

/// A frame no end of the link writes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Malformed {
    Kind(u8),
    TooLong(u32),
    /// A request or a reply without a message.
    Empty,
    /// A closing frame that carries something.
    CloseWithMessage,
}

impl std::fmt::Display for Malformed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Kind(kind) => write!(f, "a frame of unknown kind {kind}"),
            Self::TooLong(len) => {
                write!(
                    f,
                    "a message of {len} bytes, past the {MESSAGE_MAX} allowed"
                )
            }
            Self::Empty => write!(f, "a message of no bytes"),
            Self::CloseWithMessage => write!(f, "a closing frame that carries a message"),
        }
    }
}

impl std::error::Error for Malformed {}

impl Frame {
    /// The frame as it goes over the link.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let (kind, id, message): (u8, u32, &[u8]) = match self {
            Self::Request { id, message } => (REQUEST, *id, message),
            Self::Reply { id, message } => (REPLY, *id, message),
            Self::Close => (CLOSE, 0, &[]),
        };
        let len = u32::try_from(message.len()).unwrap_or(u32::MAX);
        let mut out = Vec::with_capacity(HEADER_LEN + message.len());
        out.push(kind);
        out.extend_from_slice(&id.to_be_bytes());
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(message);
        out
    }

    /// The frame a header and its message make.
    #[must_use]
    pub fn assemble(header: Header, message: Vec<u8>) -> Self {
        match header.kind {
            Kind::Request => Self::Request {
                id: header.id,
                message,
            },
            Kind::Reply => Self::Reply {
                id: header.id,
                message,
            },
            Kind::Close => Self::Close,
        }
    }
}

impl Header {
    /// Read a header, refusing what no end writes, so a reader never
    /// allocates for a length it would not accept.
    pub fn parse(bytes: &[u8; HEADER_LEN]) -> Result<Self, Malformed> {
        let id = u32::from_be_bytes([bytes[1], bytes[2], bytes[3], bytes[4]]);
        let len = u32::from_be_bytes([bytes[5], bytes[6], bytes[7], bytes[8]]);
        let kind = match bytes[0] {
            REQUEST => Kind::Request,
            REPLY => Kind::Reply,
            CLOSE => Kind::Close,
            other => return Err(Malformed::Kind(other)),
        };
        match kind {
            Kind::Close if len != 0 => Err(Malformed::CloseWithMessage),
            Kind::Close => Ok(Self { kind, id, len }),
            _ => message_len(len).map(|len| Self { kind, id, len }),
        }
    }
}

/// Check the length an agent message announces, in a frame or in the length
/// prefix the agent protocol puts before each message.
pub fn message_len(len: u32) -> Result<u32, Malformed> {
    match len {
        0 => Err(Malformed::Empty),
        len if len > MESSAGE_MAX => Err(Malformed::TooLong(len)),
        len => Ok(len),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(bytes: &[u8]) -> Result<Header, Malformed> {
        Header::parse(bytes[..HEADER_LEN].try_into().unwrap())
    }

    #[test]
    fn frames_come_back_as_they_went() {
        for frame in [
            Frame::Request {
                id: 7,
                message: vec![11],
            },
            Frame::Reply {
                id: u32::MAX,
                message: vec![14, 0, 0, 0, 1, 42],
            },
            Frame::Close,
        ] {
            let bytes = frame.encode();
            let parsed = header(&bytes).unwrap();
            assert_eq!(parsed.len as usize, bytes.len() - HEADER_LEN);
            assert_eq!(Frame::assemble(parsed, bytes[HEADER_LEN..].to_vec()), frame);
        }
    }

    #[test]
    fn the_header_is_kind_id_and_length() {
        let bytes = Frame::Request {
            id: 0x0102_0304,
            message: vec![11, 12],
        }
        .encode();
        assert_eq!(bytes, [1, 1, 2, 3, 4, 0, 0, 0, 2, 11, 12]);
    }

    #[test]
    fn what_no_end_writes_is_refused() {
        assert_eq!(
            header(&[9, 0, 0, 0, 0, 0, 0, 0, 1]),
            Err(Malformed::Kind(9))
        );
        assert_eq!(header(&[1, 0, 0, 0, 0, 0, 0, 0, 0]), Err(Malformed::Empty));
        assert_eq!(
            header(&[3, 0, 0, 0, 0, 0, 0, 0, 1]),
            Err(Malformed::CloseWithMessage)
        );
        let past = (MESSAGE_MAX + 1).to_be_bytes();
        let mut bytes = vec![2, 0, 0, 0, 1];
        bytes.extend_from_slice(&past);
        assert_eq!(header(&bytes), Err(Malformed::TooLong(MESSAGE_MAX + 1)));
        let at = MESSAGE_MAX.to_be_bytes();
        let mut bytes = vec![2, 0, 0, 0, 1];
        bytes.extend_from_slice(&at);
        assert_eq!(header(&bytes).unwrap().len, MESSAGE_MAX);
    }

    #[test]
    fn the_hello_names_the_link_and_its_version() {
        assert_eq!(HELLO.len(), 20);
        assert!(HELLO.ends_with(b" 1\n"));
    }
}
