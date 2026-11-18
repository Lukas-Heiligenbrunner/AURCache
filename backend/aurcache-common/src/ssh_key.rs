//! Names of the SSH keys a worker holds for builds.
//!
//! A name is an abstract label, never a path: the server's `build_ssh_key`
//! setting names a key, a job carries the name, and a worker maps it to a key
//! only through its own configuration. The rule below keeps every name short
//! and plain, so one can never be read as a path and is safe to show in a log
//! or the UI.

/// The name of the key a worker holds without naming any: `WORKER_GIT_SSH_KEY`,
/// or the key the worker generates when that is unset.
pub const DEFAULT: &str = "default";

/// Longest a name may be.
pub const MAX_LEN: usize = 64;

/// Check that `name` is a key name: a lowercase ASCII letter or digit, then up
/// to 63 more of those, `_` or `-`.
///
/// # Errors
/// A message saying what a name may be made of.
pub fn validate_name(name: &str) -> Result<(), String> {
    let mut chars = name.chars();
    let first_ok = chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    let rest_ok =
        chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
    if first_ok && rest_ok && name.len() <= MAX_LEN {
        Ok(())
    } else {
        Err(format!(
            "{name:?} is not an SSH key name (expected a lowercase letter or digit, then up to \
             {} more of those, `_` or `-`, such as `{DEFAULT}`)",
            MAX_LEN - 1
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_lowercase_names_are_accepted() {
        for name in [
            DEFAULT,
            "gitlab",
            "epic-games",
            "key_2",
            "0",
            &"a".repeat(64),
        ] {
            assert_eq!(validate_name(name), Ok(()), "{name:?}");
        }
    }

    /// Nothing that could be read as a path, an option or a shell word.
    #[test]
    fn anything_else_is_refused() {
        for name in [
            "",
            "Default",
            "../etc/passwd",
            "/run/secrets/key",
            "a/b",
            "a.b",
            "-rf",
            "_x",
            "my key",
            "key\n",
            "clé",
            &"a".repeat(65),
        ] {
            assert!(validate_name(name).is_err(), "{name:?}");
        }
    }
}
