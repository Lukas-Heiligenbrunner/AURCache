//! POSIX shell argument rendering, and the one ssh command line that is
//! rendered for a shell. Pass argv directly when no shell is needed.

use std::path::Path;

/// Render one argument so a POSIX shell recovers exactly its original bytes.
#[must_use]
pub fn quote_argument(argument: &str) -> String {
    if !argument.is_empty()
        && argument
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_=:./,@+".contains(c))
    {
        argument.to_owned()
    } else {
        format!("'{}'", argument.replace('\'', "'\"'\"'"))
    }
}

/// What ssh is told beside a configured `known_hosts`, so that no other source
/// of host keys can admit a host the file does not name: no other
/// `known_hosts` (the system's `/etc/ssh/ssh_known_hosts` included), no
/// `KnownHostsCommand`, no key from DNS, and no host accepted on first use.
///
/// Options on ssh's command line win over every `ssh_config`, the system's and
/// the build user's, so none of them can turn any of this back on.
pub const PINNED_HOST_KEYS: [&str; 4] = [
    "GlobalKnownHostsFile=/dev/null",
    "KnownHostsCommand=none",
    "VerifyHostKeyDNS=no",
    "StrictHostKeyChecking=yes",
];

/// An `ssh` command line, as one string for a shell, that trusts the hosts
/// `known_hosts` names and no others ([`PINNED_HOST_KEYS`]): what a build's
/// `GIT_SSH_COMMAND` is when the worker gives it host keys.
///
/// The path is quoted for the shell git runs the command through: unquoted,
/// one with a space would split into two ssh arguments.
#[must_use]
pub fn pinned_ssh_command(known_hosts: &Path) -> String {
    let mut command = String::from("ssh -o UserKnownHostsFile=");
    command.push_str(&quote_argument(&known_hosts.to_string_lossy()));
    for option in PINNED_HOST_KEYS {
        command.push_str(" -o ");
        command.push_str(option);
    }
    command
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn shell_recovers_each_argument_without_expansion() {
        for argument in ["", "hello", "a b", "a'b", "\"$HOME`id`$(id);\n*\\", "λ"] {
            let output = std::process::Command::new("sh")
                .args(["-c", &format!("printf '%s' {}", quote_argument(argument))])
                .output()
                .unwrap();
            assert!(output.status.success());
            assert_eq!(output.stdout, argument.as_bytes());
        }
    }

    /// The pin holds against an `ssh_config` that trusts more: another
    /// `known_hosts`, a `KnownHostsCommand`, DNS, or any host on first use.
    /// Asked by `ssh -G`, which prints the configuration ssh would connect
    /// with and connects to nothing.
    #[test]
    fn a_pinned_known_hosts_wins_over_any_ssh_config() {
        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join("ssh_config");
        std::fs::write(
            &config,
            "Host *\n  StrictHostKeyChecking no\n  UserKnownHostsFile /elsewhere\n  \
             GlobalKnownHostsFile /etc/ssh/ssh_known_hosts\n  \
             KnownHostsCommand /bin/cat /elsewhere\n  VerifyHostKeyDNS yes\n",
        )
        .unwrap();
        let pinned = tmp.path().join("known_hosts");
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!(
                "{} -F {} -G example.invalid",
                pinned_ssh_command(&pinned),
                quote_argument(&config.display().to_string())
            ))
            .output()
            .expect("sh should run");
        assert!(
            out.status.success(),
            "ssh -G failed (is openssh installed?): {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let effective = String::from_utf8_lossy(&out.stdout);
        let lines: Vec<&str> = effective.lines().collect();
        for expected in [
            format!("userknownhostsfile {}", pinned.display()),
            "globalknownhostsfile /dev/null".to_string(),
            "stricthostkeychecking true".to_string(),
            "verifyhostkeydns false".to_string(),
        ] {
            assert!(
                lines.contains(&expected.as_str()),
                "{expected}: {effective}"
            );
        }
        assert!(
            !lines
                .iter()
                .any(|line| line.starts_with("knownhostscommand")),
            "{effective}"
        );
    }

    /// The known-hosts path travels inside `GIT_SSH_COMMAND`, which git
    /// splits on spaces: it must arrive there as one shell word.
    #[test]
    fn a_spaced_known_hosts_path_stays_one_ssh_argument() {
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!(
                "printf '<%s>' {}",
                pinned_ssh_command(Path::new("/my dir/known hosts"))
            ))
            .output()
            .expect("sh should run");
        assert!(out.status.success());
        // The path one word, intact: the shell consumed the quotes while
        // splitting, which is exactly what protects the spaces.
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "<ssh><-o><UserKnownHostsFile=/my dir/known hosts>\
             <-o><GlobalKnownHostsFile=/dev/null><-o><KnownHostsCommand=none>\
             <-o><VerifyHostKeyDNS=no><-o><StrictHostKeyChecking=yes>"
        );
    }
}
