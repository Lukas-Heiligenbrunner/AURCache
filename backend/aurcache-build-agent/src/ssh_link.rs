//! The SSH link's wire format, which the build agent and the worker share:
//! each end sends [`HELLO`], then requests and answers go one at a time as SSH
//! agent protocol messages (a big-endian `u32` length, then that many bytes).
//! See "The SSH link" in `design/in-progress/container-workers.md`.

use std::io;

/// What each end sends first.
pub const HELLO: &[u8; 20] = b"aurcache-ssh-link 1\n";

/// The longest agent message either way, as OpenSSH's own agent limits it.
pub const MESSAGE_MAX: u32 = 256 * 1024;

/// The length that a message's prefix announces, refused when it is empty or
/// past [`MESSAGE_MAX`], so that a reader never allocates for a length that
/// it would not accept.
pub fn message_len(len: u32) -> io::Result<usize> {
    if len == 0 || len > MESSAGE_MAX {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("an agent message of {len} bytes"),
        ));
    }
    usize::try_from(len).map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_message_is_neither_empty_nor_past_the_limit() {
        assert!(message_len(0).is_err());
        assert!(message_len(MESSAGE_MAX + 1).is_err());
        assert_eq!(message_len(1).unwrap(), 1);
        assert_eq!(message_len(MESSAGE_MAX).unwrap(), 256 * 1024);
    }
}
