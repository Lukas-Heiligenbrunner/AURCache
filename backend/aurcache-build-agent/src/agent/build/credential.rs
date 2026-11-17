//! The worker's SSH key, for the download of the sources.
//!
//! The key is never in the container. The worker holds it and answers for
//! it over the link ([`super::relay`]); what the build gets is a socket that
//! reaches it, for as long as makepkg downloads the sources and no longer.
//! What runs as root afterwards, dependencies' install scriptlets among it,
//! finds neither a key nor a way to ask for a signature.

use super::output::Output;
use super::run;
use std::fs::Permissions;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

/// The hosts the build's ssh trusts, where the build user can read them.
pub const KNOWN_HOSTS: &str = "known_hosts";

/// Install what the download uses where the image has none: ssh, and git,
/// the transport of nearly every source that needs a key. As root, before
/// the build user exists. Any other VCS client comes with the build's own
/// dependencies, afterwards.
pub fn provide(output: &Output) -> Result<(), String> {
    let mut pacman = Command::new("pacman");
    pacman.args([
        "-S",
        "--needed",
        "--noconfirm",
        "--noprogressbar",
        "--color",
        "never",
        "openssh",
        "git",
    ]);
    let status = run::logged(output, &mut pacman)
        .map_err(|e| format!("pacman could not be started: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "installing openssh and git: pacman {}",
            run::describe(status)
        ))
    }
}

/// Write the hosts' keys to a file of root's that anyone may read.
pub fn known_hosts(hosts: &str, to: &Path) -> Result<PathBuf, String> {
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .custom_flags(libc::O_NOFOLLOW)
        .mode(0o644)
        .open(to)
        .and_then(|mut file| {
            file.write_all(hosts.as_bytes())?;
            file.set_permissions(Permissions::from_mode(0o644))
        });
    written.map_err(|e| format!("writing {}: {e}", to.display()))?;
    Ok(to.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_keys_are_written_for_anyone_to_read() {
        let dir = tempfile::tempdir().unwrap();
        let out = known_hosts(
            "github.com ssh-ed25519 AAAA\n",
            &dir.path().join(KNOWN_HOSTS),
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(&out).unwrap(),
            "github.com ssh-ed25519 AAAA\n"
        );
        let mode = std::fs::metadata(&out).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o644);
    }

    #[test]
    fn host_keys_are_not_written_through_a_link() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        std::fs::write(&target, "kept").unwrap();
        let link = dir.path().join(KNOWN_HOSTS);
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(known_hosts("github.com ssh-ed25519 AAAA\n", &link).is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "kept");
    }
}
