//! A short name for one worker, for what it creates where other workers
//! create things too: containers on a daemon they share.

use anyhow::{Result, bail};

/// How many hex digits of the fingerprint make the tag.
const DIGITS: usize = 16;

/// The first sixteen hex digits of a worker's fingerprint, lower case and
/// without separators.
///
/// Build ids are only unique within one server, so two workers of different
/// servers sharing an engine would otherwise name their containers alike,
/// and each would take the other's for a leftover of its own.
pub fn of(fingerprint: &str) -> Result<String> {
    let digits: String = fingerprint
        .chars()
        .filter(char::is_ascii_hexdigit)
        .map(|c| c.to_ascii_lowercase())
        .take(DIGITS)
        .collect();
    if digits.len() < DIGITS {
        bail!("fingerprint {fingerprint:?} has fewer than {DIGITS} hex digits");
    }
    Ok(digits)
}

#[cfg(test)]
mod tests {
    use super::of;

    #[test]
    fn the_tag_is_the_fingerprints_first_digits() {
        assert_eq!(
            of("AB:CD:EF:01:23:45:67:89:ab:cd:ef:01").unwrap(),
            "abcdef0123456789"
        );
        assert_eq!(
            of("0123456789abcdef0123456789abcdef").unwrap(),
            "0123456789abcdef"
        );
    }

    #[test]
    fn a_fingerprint_too_short_to_tell_workers_apart_is_refused() {
        let error = of("ab:cd").unwrap_err().to_string();
        assert!(error.contains("fewer than 16"), "{error}");
    }
}
