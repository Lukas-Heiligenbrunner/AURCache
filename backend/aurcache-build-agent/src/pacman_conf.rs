//! What a job's `pacman.conf` may say.
//!
//! The agent writes the job's `pacman.conf` into `/etc` as root, so it refuses
//! one that sets what the agent decides itself (`CacheDir`, `DBPath`,
//! `RootDir`, `GPGDir`, `HookDir`, `LogFile`) or points a server at the
//! container's own files (`file://`).

use std::fmt;

/// The options that say where pacman keeps and finds its own files.
const FORBIDDEN: [&str; 6] = [
    "CacheDir", "DBPath", "RootDir", "GPGDir", "HookDir", "LogFile",
];

/// The longest line accepted. pacman reads its configuration into a fixed
/// buffer and takes what does not fit as the next line, so a directive at the
/// end of a long enough comment is one it would read and this check would not.
const LINE_MAX: usize = 1024;

/// Why a `pacman.conf` was refused: an option the agent sets itself, or a
/// `file://` server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfRefusal {
    /// An option whose value is not the job's to choose, as it was written.
    Option { name: String },
    /// A server on the container's own filesystem.
    FileServer { url: String },
    /// A line too long to be sure pacman reads it as one; numbered from 1.
    LongLine { number: usize },
}

impl fmt::Display for ConfRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Option { name } => write!(f, "`{name}` is not a job's to set"),
            Self::FileServer { url } => {
                write!(f, "`{url}` is a server on the container's own filesystem")
            }
            Self::LongLine { number } => {
                write!(f, "line {number} is longer than {LINE_MAX} bytes")
            }
        }
    }
}

impl std::error::Error for ConfRefusal {}

/// Accept `text` as a job's `pacman.conf`, or say why not.
///
/// Read the way pacman reads it: a line at a time, surrounding whitespace
/// dropped, `#` starting a comment only at the start of a line, a directive
/// split at its first `=`. Names are compared without regard to case, which
/// refuses more than pacman would honour.
pub fn check(text: &str) -> Result<(), ConfRefusal> {
    for (index, raw) in text.split('\n').enumerate() {
        if raw.len() > LINE_MAX {
            return Err(ConfRefusal::LongLine { number: index + 1 });
        }
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('[') {
            continue;
        }
        let (name, value) = match line.split_once('=') {
            Some((name, value)) => (name.trim(), value.trim()),
            None => (line, ""),
        };
        if FORBIDDEN.iter().any(|f| f.eq_ignore_ascii_case(name)) {
            return Err(ConfRefusal::Option {
                name: name.to_string(),
            });
        }
        if name.eq_ignore_ascii_case("Server") && is_local(value) {
            return Err(ConfRefusal::FileServer {
                url: value.to_string(),
            });
        }
    }
    Ok(())
}

/// Whether a server's URL has the `file` scheme, however it is written.
fn is_local(url: &str) -> bool {
    url.get(..5)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("file:"))
}

#[cfg(test)]
mod tests {
    use super::{ConfRefusal, LINE_MAX, check};

    /// Arch's `/etc/pacman.conf` as the `pacman` package ships it, comments
    /// included: the forbidden options appear in it, commented out.
    const STOCK: &str = "\
#
# /etc/pacman.conf
#
# See the pacman.conf(5) manpage for option and repository directives

#
# GENERAL OPTIONS
#
[options]
# The following paths are commented out with their default values listed.
# If you wish to use different paths, uncomment and update the paths.
#RootDir     = /
#DBPath      = /var/lib/pacman/
#CacheDir    = /var/cache/pacman/pkg/
#LogFile     = /var/log/pacman.log
#GPGDir      = /etc/pacman.d/gnupg/
#HookDir     = /etc/pacman.d/hooks/
HoldPkg     = pacman glibc
#XferCommand = /usr/bin/curl -L -C - -f -o %o %u
#XferCommand = /usr/bin/wget --passive-ftp -c -O %o %u
#CleanMethod = KeepInstalled
Architecture = auto

# Pacman won't upgrade packages listed in IgnorePkg and members of IgnoreGroup
#IgnorePkg   =
#IgnoreGroup =

#NoUpgrade   =
#NoExtract   =

# Misc options
#UseSyslog
#Color
#NoProgressBar
CheckSpace
#VerbosePkgLists
ParallelDownloads = 5
DownloadUser = alpm
#DisableSandbox

# By default, pacman accepts packages signed by keys that its local keyring
# trusts (see pacman-key and its man page), as well as unsigned packages.
SigLevel    = Required DatabaseOptional
LocalFileSigLevel = Optional
#RemoteFileSigLevel = Required

[core]
Include = /etc/pacman.d/mirrorlist

[extra]
Include = /etc/pacman.d/mirrorlist

# If you want to run 32 bit applications on your x86_64 system,
# enable the multilib repositories as required here.

#[multilib]
#Include = /etc/pacman.d/mirrorlist

# An example of a custom package repository.  See the pacman manpage for
# tips on creating your own repositories.
#[custom]
#SigLevel = Optional TrustAll
#Server = file:///home/custompkgs
";

    #[test]
    fn the_stock_file_is_accepted() {
        assert_eq!(check(STOCK), Ok(()));
    }

    /// What the server renders for a job, with the worker's repository
    /// section after it.
    #[test]
    fn a_jobs_file_is_accepted() {
        let conf = "[options]\nDisableSandbox\nSigLevel = Never\nHoldPkg = pacman glibc\n\
                    Architecture = x86_64\n\n[core]\nInclude = /etc/pacman.d/mirrorlist\n\n\
                    [extra]\nInclude = /etc/pacman.d/mirrorlist\n\n\
                    [aurcache]\nSigLevel = Optional TrustAll\nServer = http://aurcache.test:8081/x86_64\n";
        assert_eq!(check(conf), Ok(()));
        assert_eq!(check(""), Ok(()));
        assert_eq!(
            check("Server = https://mirror.test/$repo/os/$arch\n"),
            Ok(())
        );
    }

    #[test]
    fn each_forbidden_option_is_refused_however_it_is_written() {
        for option in [
            "CacheDir", "DBPath", "RootDir", "GPGDir", "HookDir", "LogFile",
        ] {
            let spellings = [
                option.to_string(),
                option.to_lowercase(),
                option.to_uppercase(),
            ];
            for name in &spellings {
                for line in [
                    format!("{name} = /tmp/x"),
                    format!("{name}=/tmp/x"),
                    format!("   {name}   =   /tmp/x   "),
                    format!("\t{name}\t=\t/tmp/x"),
                    format!("{name} = /tmp/x\r"),
                    name.clone(),
                ] {
                    let line = line.as_str();
                    for conf in [
                        format!("[options]\n{line}\n"),
                        format!("[options]\nHoldPkg = pacman\n{line}"),
                        format!("[core]\n{line}\n"),
                        line.to_string(),
                    ] {
                        assert_eq!(
                            check(&conf),
                            Err(ConfRefusal::Option { name: name.clone() }),
                            "{conf:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn a_file_server_is_refused() {
        for (line, url) in [
            ("Server = file:///srv/repo", "file:///srv/repo"),
            ("Server=file:///srv/repo", "file:///srv/repo"),
            ("  server   =   FILE:///srv/repo  ", "FILE:///srv/repo"),
            ("SERVER = File:/srv/repo", "File:/srv/repo"),
            (
                "Server = file://localhost/srv/repo",
                "file://localhost/srv/repo",
            ),
        ] {
            let conf = format!("[options]\nArchitecture = auto\n\n[custom]\n{line}\n");
            assert_eq!(
                check(&conf),
                Err(ConfRefusal::FileServer {
                    url: url.to_string()
                }),
                "{line:?}"
            );
        }
    }

    /// A directive pacman would find after the part of a line that fills its
    /// buffer is not one this check could see.
    #[test]
    fn a_line_pacman_would_split_is_refused() {
        let hidden = format!("#{}CacheDir = /tmp/x\n", " ".repeat(5000));
        let conf = format!("[options]\n{hidden}");
        assert_eq!(check(&conf), Err(ConfRefusal::LongLine { number: 2 }));
        let longest = format!("#{}", "x".repeat(LINE_MAX - 1));
        assert_eq!(check(&longest), Ok(()));
    }

    #[test]
    fn a_refusal_says_what_it_is_about() {
        let said = check("cachedir = /x").unwrap_err().to_string();
        assert!(said.contains("`cachedir`"), "{said}");
        let said = check("Server = file:///x").unwrap_err().to_string();
        assert!(said.contains("file:///x"), "{said}");
    }
}
