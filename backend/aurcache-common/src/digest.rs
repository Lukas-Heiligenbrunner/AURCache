//! The one digest form AURCache writes: SHA-256, as lowercase hex.

use sha2::{Digest, Sha256};

/// SHA-256 of `bytes`, as 64 lowercase hex digits.
#[must_use]
pub fn sha256_hex(bytes: impl AsRef<[u8]>) -> String {
    hex::encode(Sha256::digest(bytes.as_ref()))
}

#[cfg(test)]
mod tests {
    use super::sha256_hex;

    #[test]
    fn is_lowercase_hex_of_the_digest() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
