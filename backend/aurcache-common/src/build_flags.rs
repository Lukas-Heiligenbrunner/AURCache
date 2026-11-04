//! The makepkg flags a package may be built with.
//!
//! A package's build flags are handed to `makepkg` as they are, on every
//! worker, so a flag makepkg rejects -- or one that makes it exit without a
//! package, or that takes over something the worker owns -- fails every build
//! of that package. Checked where flags come in rather than discovered there.

/// What a build does with a makepkg option.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Use {
    Allowed,
    /// Refused, and why.
    Refused(&'static str),
}

/// A makepkg option: its long form, its short letter if it has one, and what
/// a build does with it.
struct MakepkgOption {
    long: &'static str,
    short: Option<char>,
    usage: Use,
}

const fn allowed(long: &'static str, short: Option<char>) -> MakepkgOption {
    MakepkgOption {
        long,
        short,
        usage: Use::Allowed,
    }
}

const fn refused(long: &'static str, short: Option<char>, why: &'static str) -> MakepkgOption {
    MakepkgOption {
        long,
        short,
        usage: Use::Refused(why),
    }
}

const NO_PACKAGE: &str = "makepkg would produce no package";
const WORKER_OWNED: &str = "the worker sets this itself";

/// Every option of makepkg 7.1, and those of pacman it passes through.
const OPTIONS: &[MakepkgOption] = &[
    allowed("--noconfirm", None),
    allowed("--noprogressbar", None),
    allowed("--nocolor", Some('m')),
    allowed("--ignorearch", Some('A')),
    allowed("--nodeps", Some('d')),
    allowed("--force", Some('f')),
    allowed("--clean", Some('c')),
    allowed("--cleanbuild", Some('C')),
    allowed("--syncdeps", Some('s')),
    allowed("--rmdeps", Some('r')),
    allowed("--check", None),
    allowed("--nocheck", None),
    allowed("--noprepare", None),
    allowed("--holdver", None),
    allowed("--skipchecksums", None),
    allowed("--skipinteg", None),
    allowed("--skippgpcheck", None),
    allowed("--nosign", None),
    allowed("--needed", None),
    allowed("--asdeps", None),
    refused("--nobuild", Some('o'), NO_PACKAGE),
    refused("--noarchive", None, NO_PACKAGE),
    refused("--repackage", Some('R'), NO_PACKAGE),
    refused("--noextract", Some('e'), NO_PACKAGE),
    refused("--source", Some('S'), NO_PACKAGE),
    refused("--allsource", None, NO_PACKAGE),
    refused("--verifysource", None, NO_PACKAGE),
    refused("--geninteg", Some('g'), NO_PACKAGE),
    refused("--packagelist", None, NO_PACKAGE),
    refused("--printsrcinfo", None, NO_PACKAGE),
    refused("--help", Some('h'), NO_PACKAGE),
    refused("--version", Some('V'), NO_PACKAGE),
    refused("--config", None, WORKER_OWNED),
    refused("-p", None, WORKER_OWNED),
    refused("--dir", Some('D'), WORKER_OWNED),
    refused("--log", Some('L'), WORKER_OWNED),
    refused(
        "--install",
        Some('i'),
        "a build does not install what it builds",
    ),
    refused("--sign", None, "workers hold no signing key"),
    refused("--key", None, "workers hold no signing key"),
];

/// A build flag that cannot be given to a build.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refused {
    pub flag: String,
    pub reason: &'static str,
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "build flag '{}' is not accepted: {}",
            self.flag, self.reason
        )
    }
}

impl std::error::Error for Refused {}

/// The flags in their canonical form: long names, short bundles such as `-cf`
/// split, duplicates dropped, order kept.
///
/// # Errors
///
/// The first flag a build cannot take: one makepkg does not know, or one it
/// knows that would stop the build producing a package or take over what the
/// worker sets.
pub fn canonical(flags: &[impl AsRef<str>]) -> Result<Vec<String>, Refused> {
    let mut out: Vec<String> = Vec::new();
    for flag in flags {
        let flag = flag.as_ref().trim();
        if flag.is_empty() {
            continue;
        }
        for long in expand(flag)? {
            if !out.iter().any(|seen| seen == long) {
                out.push(long.to_string());
            }
        }
    }
    Ok(out)
}

/// One flag as the long options it stands for.
fn expand(flag: &str) -> Result<Vec<&'static str>, Refused> {
    let refuse = |reason| Refused {
        flag: flag.to_string(),
        reason,
    };
    let unknown = || refuse("makepkg has no such option");
    let resolve = |option: &MakepkgOption| match option.usage {
        Use::Allowed => Ok(option.long),
        Use::Refused(reason) => Err(refuse(reason)),
    };

    if flag.starts_with("--") || flag == "-p" {
        let option = OPTIONS
            .iter()
            .find(|o| o.long == flag)
            .ok_or_else(unknown)?;
        return Ok(vec![resolve(option)?]);
    }
    let letters = flag
        .strip_prefix('-')
        .filter(|l| !l.is_empty())
        .ok_or_else(unknown)?;
    letters
        .chars()
        .map(|letter| {
            let option = OPTIONS
                .iter()
                .find(|o| o.short == Some(letter))
                .ok_or_else(unknown)?;
            resolve(option)
        })
        .collect()
}

/// A 0.5.0 package's flags, as a makepkg build takes them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Upgraded {
    /// The flags kept, canonical.
    pub flags: Vec<String>,
    /// The flags dropped because no build can take them -- beyond the paru
    /// ones every 0.5.0 package carried, which are not worth reporting.
    pub dropped: Vec<String>,
}

/// Translate the flags 0.5.0 stored, which were paru's, for makepkg.
///
/// Its defaults were `--pgpfetch;-B;--noconfirm;--noprogressbar;--color never`
/// and, before that, `-Syu;…` (which its own migration rewrote to `-Byu`).
/// Both come out as the current default, `--noconfirm --noprogressbar
/// --nocolor`: the worker fetches `validpgpkeys` and refreshes the system
/// itself, and always builds.
#[must_use]
pub fn upgrade_from_paru(flags: &[impl AsRef<str>]) -> Upgraded {
    let mut upgraded = Upgraded::default();
    for flag in flags {
        let flag = flag.as_ref().trim();
        let translated = match flag {
            "" | "-Syu" | "-Byu" | "-B" | "--pgpfetch" => continue,
            "--color never" | "--color=never" => "--nocolor",
            other => other,
        };
        match canonical(&[translated]) {
            Ok(longs) => {
                for long in longs {
                    if !upgraded.flags.contains(&long) {
                        upgraded.flags.push(long);
                    }
                }
            }
            Err(_) => upgraded.dropped.push(flag.to_string()),
        }
    }
    upgraded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_bundles_become_long_flags_once() {
        assert_eq!(
            canonical(&["-cf", "--force", " --noconfirm ", ""]).unwrap(),
            ["--clean", "--force", "--noconfirm"]
        );
    }

    #[test]
    fn a_flag_that_leaves_no_package_is_refused_with_its_reason() {
        assert_eq!(
            canonical(&["--noconfirm", "-so"]),
            Err(Refused {
                flag: "-so".to_string(),
                reason: NO_PACKAGE,
            })
        );
    }

    #[test]
    fn paru_flags_are_not_makepkg_flags() {
        for flag in [
            "--pgpfetch",
            "-B",
            "--color never",
            "--color=never",
            "-",
            "x",
        ] {
            assert!(canonical(&[flag]).is_err(), "{flag} accepted");
        }
    }

    /// The two defaults 0.5.0 stored on every package it added.
    #[test]
    fn both_0_5_defaults_become_the_current_default() {
        let current = ["--noconfirm", "--noprogressbar", "--nocolor"];
        for stored in [
            "--pgpfetch;-B;--noconfirm;--noprogressbar;--color never",
            "-Byu;--noconfirm;--noprogressbar;--color never",
        ] {
            let flags: Vec<&str> = stored.split(';').collect();
            assert_eq!(
                upgrade_from_paru(&flags),
                Upgraded {
                    flags: current.map(String::from).to_vec(),
                    dropped: vec![],
                },
                "{stored}"
            );
        }
    }

    #[test]
    fn what_no_build_can_take_is_reported() {
        let upgraded = upgrade_from_paru(&["--noconfirm", "--skipreview", "-i", "-cf"]);
        assert_eq!(upgraded.flags, ["--noconfirm", "--clean", "--force"]);
        assert_eq!(upgraded.dropped, ["--skipreview", "-i"]);
    }
}
