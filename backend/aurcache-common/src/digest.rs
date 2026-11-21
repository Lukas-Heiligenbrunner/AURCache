//! The one digest form AURCache writes: SHA-256, as lowercase hex.

use sha2::{Digest, Sha256};

/// SHA-256 of `bytes`, as 64 lowercase hex digits.
#[must_use]
pub fn sha256_hex(bytes: impl AsRef<[u8]>) -> String {
    hex::encode(Sha256::digest(bytes.as_ref()))
}

/// SHA-256 of everything `reader` yields, as [`sha256_hex`] gives it, read a
/// piece at a time: for a file too large to hold in memory.
pub fn sha256_hex_of(mut reader: impl std::io::Read) -> std::io::Result<String> {
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; 64 * 1024];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => return Ok(hex::encode(hasher.finalize())),
            Ok(read) => hasher.update(&buffer[..read]),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{sha256_hex, sha256_hex_of};

    #[test]
    fn is_lowercase_hex_of_the_digest() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn of_a_reader_is_of_what_it_yields() {
        let bytes = vec![7_u8; 200 * 1024];
        assert_eq!(sha256_hex_of(bytes.as_slice()).unwrap(), sha256_hex(&bytes));
    }
}
