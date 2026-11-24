//! How a worker that reaches its build container only through `exec` hands
//! the job in: the stream that `receive` reads from its stdin.
//!
//! Each input is its length in decimal and a newline, then that many bytes:
//! the spec, then the source archive. The stream says where it ends, so
//! `receive` never waits for its stdin to close, which an exec stream does
//! not always pass on.

use std::io::{self, BufRead, Read};

/// The most digits a length has: `u64::MAX` has twenty.
const DIGITS: u64 = 20;

/// What comes before an input of `len` bytes.
#[must_use]
pub fn prefix(len: usize) -> String {
    format!("{len}\n")
}

/// Read the length that comes before an input.
pub fn read_len(from: &mut impl BufRead) -> io::Result<u64> {
    let mut line = Vec::new();
    Read::take(&mut *from, DIGITS + 1).read_until(b'\n', &mut line)?;
    let digits = line
        .strip_suffix(b"\n")
        .ok_or_else(|| invalid("no length"))?;
    std::str::from_utf8(digits)
        .ok()
        .filter(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|digits| digits.parse().ok())
        .ok_or_else(|| invalid("a length that is not a number"))
}

fn invalid(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("the job has {what}"))
}

#[cfg(test)]
mod tests {
    use super::{prefix, read_len};

    #[test]
    fn a_length_reads_back_and_leaves_the_bytes_after_it() {
        let stream = format!("{}rest", prefix(1234)).into_bytes();
        let mut reader = stream.as_slice();
        assert_eq!(read_len(&mut reader).unwrap(), 1234);
        assert_eq!(reader, b"rest");
    }

    #[test]
    fn anything_but_digits_and_a_newline_is_refused() {
        for bad in [
            &b""[..],
            b"\n",
            b"12",
            b"-1\n",
            b"+1\n",
            b"1 \n",
            b"123456789012345678901\n",
        ] {
            assert!(read_len(&mut &bad[..]).is_err(), "{bad:?}");
        }
    }
}
