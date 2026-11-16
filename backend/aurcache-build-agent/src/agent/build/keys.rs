//! The job's PGP keys, fetched into the build user's own keyring.

use super::output::Output;
use super::run;
use super::user::BuildUser;

/// Fetch each of the job's keys from the keyserver, as the build user.
///
/// A key that cannot be fetched stops the build before it starts: whether
/// the keyserver is down or the key is not on it cannot be told apart here,
/// and a build that needs the key would only fail later for a reason that
/// looks like the package's.
pub fn import(
    keys: &[String],
    keyserver: &str,
    user: &BuildUser,
    output: &Output,
) -> Result<(), String> {
    for key in keys {
        if !is_fingerprint(key) {
            output.say(&format!(
                "skipping `{key}`, which is not a PGP key fingerprint"
            ));
            continue;
        }
        let mut gpg = user.command("gpg");
        gpg.args(["--batch", "--keyserver", keyserver, "--recv-keys", key]);
        let status =
            run::logged(output, &mut gpg).map_err(|e| format!("gpg could not be started: {e}"))?;
        if !status.success() {
            return Err(format!(
                "fetching PGP key {key} from {keyserver}: gpg {}",
                run::describe(status)
            ));
        }
    }
    Ok(())
}

/// Whether a word is a key id or fingerprint as `validpgpkeys` holds them:
/// hexadecimal, of a length gpg takes.
fn is_fingerprint(key: &str) -> bool {
    (8..=64).contains(&key.len()) && key.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::is_fingerprint;

    #[test]
    fn only_hexadecimal_is_handed_to_gpg() {
        assert!(is_fingerprint("ABAF11C65A2970B130ABE3C479BE3E4300411886"));
        assert!(is_fingerprint("79be3e4300411886"));
        assert!(!is_fingerprint(""));
        assert!(!is_fingerprint("--import"));
        assert!(!is_fingerprint("ABAF11C6 5A2970B1"));
        assert!(!is_fingerprint("someone@example.test"));
        assert!(!is_fingerprint("ABC"));
    }
}
