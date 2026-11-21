//! The makepkg flags a package may be built with.
//!
//! A package's build flags are handed to `makepkg` on every worker, so a flag
//! makepkg rejects -- or one that makes it exit without a package, or that
//! takes over something the worker owns -- fails every build of that package.
//! Checked where flags come in rather than discovered there.
//!
//! Of the options a build can take, only those in [`OFFERED`] are a choice a
//! package makes, and only those are stored. The rest are accepted and
//! forgotten, so that a list naming one is not refused: they change nothing
//! on a worker, or undo what a package sets elsewhere.

/// What a build does with a makepkg option.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Use {
    /// A choice a package makes, listed in [`OFFERED`].
    Offered,
    /// The same as giving these offered options.
    Expands(&'static [&'static str]),
    /// Accepted, and not stored: a worker gives every build this already, it
    /// changes nothing in a build, or it undoes what a package sets elsewhere.
    Dropped,
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

const fn offered(long: &'static str, short: Option<char>) -> MakepkgOption {
    MakepkgOption {
        long,
        short,
        usage: Use::Offered,
    }
}

const fn expands(long: &'static str, into: &'static [&'static str]) -> MakepkgOption {
    MakepkgOption {
        long,
        short: None,
        usage: Use::Expands(into),
    }
}

const fn dropped(long: &'static str, short: Option<char>) -> MakepkgOption {
    MakepkgOption {
        long,
        short,
        usage: Use::Dropped,
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
    offered("--nocheck", None),
    offered("--noprepare", None),
    offered("--holdver", None),
    offered("--ignorearch", Some('A')),
    offered("--nodeps", Some('d')),
    offered("--skipchecksums", None),
    offered("--skippgpcheck", None),
    expands("--skipinteg", &["--skipchecksums", "--skippgpcheck"]),
    // What every build needs, and the worker gives it: see [`EVERY_BUILD`].
    dropped("--noconfirm", None),
    dropped("--noprogressbar", None),
    dropped("--nocolor", Some('m')),
    // Both workers install the dependencies themselves.
    dropped("--syncdeps", Some('s')),
    // `check()` runs unless `--nocheck` is given.
    dropped("--check", None),
    // A build starts from an empty tree, or from the kept one the package
    // chose to keep: these would only undo that choice.
    dropped("--clean", Some('c')),
    dropped("--cleanbuild", Some('C')),
    // The chroot is new, and thrown away afterwards.
    dropped("--force", Some('f')),
    dropped("--rmdeps", Some('r')),
    // Only apply to `--install`, which is refused.
    dropped("--needed", None),
    dropped("--asdeps", None),
    // Workers hold no signing key.
    dropped("--nosign", None),
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

/// A build flag a package can choose, and what it does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Offered {
    /// The long option, as makepkg takes it and a package stores it.
    pub flag: &'static str,
    /// What it does to a build, in a line.
    pub summary: &'static str,
}

/// Every flag a package can choose, in the order a package's list is kept in.
pub const OFFERED: &[Offered] = &[
    Offered {
        flag: "--nocheck",
        summary: "Skip the check() tests, and the checkdepends they need",
    },
    Offered {
        flag: "--noprepare",
        summary: "Skip prepare(), which patches and sets up the sources",
    },
    Offered {
        flag: "--holdver",
        summary: "Build VCS sources as they were fetched, without pulling newer commits",
    },
    Offered {
        flag: "--ignorearch",
        summary: "Build even when the PKGBUILD's arch does not list this architecture",
    },
    Offered {
        flag: "--nodeps",
        summary: "Check and install no dependencies before the build",
    },
    Offered {
        flag: "--skipchecksums",
        summary: "Do not verify the sources against their checksums",
    },
    Offered {
        flag: "--skippgpcheck",
        summary: "Do not verify the sources' PGP signatures",
    },
];

/// The flags a worker gives every build, whatever the package chose: nobody is
/// there to answer a prompt, and a build log is not a terminal.
pub const EVERY_BUILD: &[&str] = &["--noconfirm", "--noprogressbar", "--nocolor"];

/// What a worker hands makepkg for a job's `flags`: [`EVERY_BUILD`], then the
/// job's own flags, less any copy of those.
#[must_use]
pub fn for_makepkg(flags: &[impl AsRef<str>]) -> Vec<String> {
    EVERY_BUILD
        .iter()
        .map(|flag| (*flag).to_string())
        .chain(
            flags
                .iter()
                .map(|flag| flag.as_ref().trim())
                .filter(|flag| !flag.is_empty() && !EVERY_BUILD.contains(flag))
                .map(str::to_string),
        )
        .collect()
}

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

/// What a build does with one option a flag names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Kept, as these offered flags: the option itself, or the ones it stands
    /// for.
    Kept(&'static [&'static str]),
    /// Accepted, and not stored.
    Dropped,
    /// A build cannot take it, and why.
    Refused(&'static str),
}

/// One option a list of flags names, and what a build does with it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Classified {
    /// The option's long form, or, where makepkg has no such option, what was
    /// given: `-X` for a letter of a bundle, the whole flag otherwise.
    pub option: String,
    pub kind: Kind,
}

const NO_SUCH_OPTION: &str = "makepkg has no such option";

/// Every option `flags` name, in order: a short bundle such as `-cd` is one
/// option per letter.
#[must_use]
pub fn classify(flags: &[impl AsRef<str>]) -> Vec<Classified> {
    flags
        .iter()
        .flat_map(|flag| options_of(flag.as_ref().trim()))
        .collect()
}

/// The offered flags `options` keep, each once, in the order of [`OFFERED`].
#[must_use]
pub fn kept(options: &[Classified]) -> Vec<String> {
    OFFERED
        .iter()
        .filter(|offered| {
            options.iter().any(
                |option| matches!(option.kind, Kind::Kept(flags) if flags.contains(&offered.flag)),
            )
        })
        .map(|offered| offered.flag.to_string())
        .collect()
}

/// The flags in their canonical form: the offered options they amount to,
/// each once, in the order of [`OFFERED`]. Short bundles such as `-dA` are
/// split, `--skipinteg` stands for the two checks it skips, and an option a
/// package does not choose is left out.
///
/// # Errors
///
/// The first flag a build cannot take: one makepkg does not know, or one it
/// knows that would stop the build producing a package or take over what the
/// worker sets.
pub fn canonical(flags: &[impl AsRef<str>]) -> Result<Vec<String>, Refused> {
    let mut options = Vec::new();
    for flag in flags {
        let flag = flag.as_ref().trim();
        let named = options_of(flag);
        let refusal = named.iter().find_map(|option| match option.kind {
            Kind::Refused(reason) => Some(reason),
            Kind::Kept(_) | Kind::Dropped => None,
        });
        if let Some(reason) = refusal {
            return Err(Refused {
                flag: flag.to_string(),
                reason,
            });
        }
        options.extend(named);
    }
    Ok(kept(&options))
}

/// The options one flag names: one for a long option, one per letter of a
/// short bundle, none for an empty flag.
fn options_of(flag: &str) -> Vec<Classified> {
    if flag.is_empty() {
        return Vec::new();
    }
    if flag.starts_with("--") || flag == "-p" {
        let option = OPTIONS.iter().find(|o| o.long == flag);
        return vec![option.map_or_else(|| unknown(flag.to_string()), classified)];
    }
    let Some(letters) = flag.strip_prefix('-').filter(|l| !l.is_empty()) else {
        return vec![unknown(flag.to_string())];
    };
    letters
        .chars()
        .map(|letter| {
            let option = OPTIONS.iter().find(|o| o.short == Some(letter));
            option.map_or_else(|| unknown(format!("-{letter}")), classified)
        })
        .collect()
}

fn classified(option: &'static MakepkgOption) -> Classified {
    Classified {
        option: option.long.to_string(),
        kind: match option.usage {
            Use::Offered => Kind::Kept(std::slice::from_ref(&option.long)),
            Use::Expands(into) => Kind::Kept(into),
            Use::Dropped => Kind::Dropped,
            Use::Refused(reason) => Kind::Refused(reason),
        },
    }
}

fn unknown(given: String) -> Classified {
    Classified {
        option: given,
        kind: Kind::Refused(NO_SUCH_OPTION),
    }
}

/// A 0.5.0 package's flags, as a makepkg build takes them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Upgraded {
    /// The flags kept, canonical.
    pub flags: Vec<String>,
    /// The options dropped because no build can take them, each once and by
    /// its [`Classified::option`] -- beyond the paru ones every 0.5.0 package
    /// carried, which are not worth reporting.
    pub dropped: Vec<String>,
}

/// Translate the flags 0.5.0 stored, which were paru's, for makepkg.
///
/// `-Syu`, `-Byu`, `-B`, `--pgpfetch` and `--color never` are paru's spelling
/// of what every build already gets: the worker refreshes the system, always
/// builds, fetches `validpgpkeys` and passes `--nocolor` itself. They are
/// dropped and not reported, so 0.5.0's defaults,
/// `--pgpfetch;-B;--noconfirm;--noprogressbar;--color never` and
/// `-Byu;--noconfirm;--noprogressbar;--color never`, come out empty. A bundle
/// holding a letter no build can take keeps its other letters.
#[must_use]
pub fn upgrade_from_paru(flags: &[impl AsRef<str>]) -> Upgraded {
    let translated: Vec<&str> = flags
        .iter()
        .map(|flag| flag.as_ref().trim())
        .filter(|flag| {
            !matches!(
                *flag,
                "" | "-Syu" | "-Byu" | "-B" | "--pgpfetch" | "--color never" | "--color=never"
            )
        })
        .collect();
    let options = classify(&translated);
    let mut dropped: Vec<String> = Vec::new();
    for option in &options {
        if matches!(option.kind, Kind::Refused(_)) && !dropped.contains(&option.option) {
            dropped.push(option.option.clone());
        }
    }
    Upgraded {
        flags: kept(&options),
        dropped,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_bundles_become_long_flags_once() {
        assert_eq!(
            canonical(&["-dA", "--nodeps", " --nocheck ", ""]).unwrap(),
            ["--nocheck", "--ignorearch", "--nodeps"]
        );
    }

    /// Two lists making the same choices are the same list.
    #[test]
    fn flags_come_out_in_the_offered_order() {
        assert_eq!(
            canonical(&["--skippgpcheck", "--nocheck"]).unwrap(),
            canonical(&["--nocheck", "--skippgpcheck"]).unwrap(),
        );
        let every: Vec<&str> = OFFERED.iter().rev().map(|o| o.flag).collect();
        assert_eq!(
            canonical(&every).unwrap(),
            OFFERED.iter().map(|o| o.flag).collect::<Vec<_>>()
        );
    }

    #[test]
    fn skipinteg_is_both_checks_it_skips() {
        assert_eq!(
            canonical(&["--skipinteg", "--skippgpcheck"]).unwrap(),
            ["--skipchecksums", "--skippgpcheck"]
        );
    }

    /// What a build needs anyway, or what changes nothing in one, is accepted
    /// and not kept.
    #[test]
    fn options_that_change_nothing_are_dropped() {
        assert_eq!(
            canonical(&["--noconfirm", "--noprogressbar", "-m", "-sCcfr"]).unwrap(),
            Vec::<String>::new()
        );
        assert_eq!(
            canonical(&["--cleanbuild", "--nocheck", "--nosign"]).unwrap(),
            ["--nocheck"]
        );
    }

    /// Every option a build can take is accepted, offered or not: a dump
    /// naming a refused one is refused whole.
    #[test]
    fn every_option_a_build_can_take_is_accepted() {
        for flag in [
            "--noconfirm",
            "--noprogressbar",
            "--nocolor",
            "--ignorearch",
            "--nodeps",
            "--force",
            "--clean",
            "--cleanbuild",
            "--syncdeps",
            "--rmdeps",
            "--check",
            "--nocheck",
            "--noprepare",
            "--holdver",
            "--skipchecksums",
            "--skipinteg",
            "--skippgpcheck",
            "--nosign",
            "--needed",
            "--asdeps",
            "-m",
            "-A",
            "-d",
            "-f",
            "-c",
            "-C",
            "-s",
            "-r",
        ] {
            assert!(canonical(&[flag]).is_ok(), "{flag} refused");
        }
    }

    /// A refused option fails a whole dump that names it, so the set only
    /// changes when makepkg's options do.
    #[test]
    fn the_refused_options_are_these() {
        let refused: Vec<&str> = OPTIONS
            .iter()
            .filter(|o| matches!(o.usage, Use::Refused(_)))
            .map(|o| o.long)
            .collect();
        assert_eq!(
            refused,
            [
                "--nobuild",
                "--noarchive",
                "--repackage",
                "--noextract",
                "--source",
                "--allsource",
                "--verifysource",
                "--geninteg",
                "--packagelist",
                "--printsrcinfo",
                "--help",
                "--version",
                "--config",
                "-p",
                "--dir",
                "--log",
                "--install",
                "--sign",
                "--key",
            ]
        );
    }

    /// The table of options and the list a package chooses from agree.
    #[test]
    fn the_offered_options_are_the_offered_list() {
        let in_table: Vec<&str> = OPTIONS
            .iter()
            .filter(|o| o.usage == Use::Offered)
            .map(|o| o.long)
            .collect();
        let listed: Vec<&str> = OFFERED.iter().map(|o| o.flag).collect();
        assert_eq!(in_table, listed);
        for option in OPTIONS {
            if let Use::Expands(into) = option.usage {
                for flag in into {
                    assert!(listed.contains(flag), "{} expands to {flag}", option.long);
                }
            }
        }
    }

    #[test]
    fn a_summary_is_one_short_line() {
        for Offered { flag, summary } in OFFERED {
            assert!(
                !summary.is_empty() && summary.len() <= 80 && !summary.contains('\n'),
                "{flag}: {summary:?}"
            );
            assert!(!summary.ends_with('.'), "{flag}: {summary:?}");
        }
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

    /// The worker's own flags come first, and a copy the server sent as well
    /// is not given twice.
    #[test]
    fn makepkg_gets_every_build_flag_once() {
        assert_eq!(
            for_makepkg(&["--noconfirm", "--nocheck", "--nocolor", "--noprogressbar"]),
            ["--noconfirm", "--noprogressbar", "--nocolor", "--nocheck"]
        );
        assert_eq!(for_makepkg(&[] as &[&str]), EVERY_BUILD);
    }

    /// The two defaults 0.5.0 stored on every package it added.
    #[test]
    fn both_0_5_defaults_become_empty() {
        for stored in [
            "--pgpfetch;-B;--noconfirm;--noprogressbar;--color never",
            "-Byu;--noconfirm;--noprogressbar;--color never",
        ] {
            let flags: Vec<&str> = stored.split(';').collect();
            assert_eq!(upgrade_from_paru(&flags), Upgraded::default(), "{stored}");
        }
    }

    #[test]
    fn what_no_build_can_take_is_reported() {
        let upgraded =
            upgrade_from_paru(&["--noconfirm", "--skipreview", "-i", "-cdA", "--nocheck"]);
        assert_eq!(upgraded.flags, ["--nocheck", "--ignorearch", "--nodeps"]);
        assert_eq!(upgraded.dropped, ["--skipreview", "--install"]);
    }

    /// A bundle holding a letter no build can take keeps its other letters.
    #[test]
    fn an_upgraded_bundle_loses_only_its_refused_letters() {
        let upgraded = upgrade_from_paru(&["-dXo", "-o"]);
        assert_eq!(upgraded.flags, ["--nodeps"]);
        assert_eq!(upgraded.dropped, ["-X", "--nobuild"]);
    }

    /// A bundle is one option per letter, each by its long form.
    #[test]
    fn a_bundle_is_classified_letter_by_letter() {
        let option = |option: &str, kind| Classified {
            option: option.to_string(),
            kind,
        };
        assert_eq!(
            classify(&["-cdo", " --skipinteg ", "", "-X", "x"]),
            [
                option("--clean", Kind::Dropped),
                option("--nodeps", Kind::Kept(&["--nodeps"])),
                option("--nobuild", Kind::Refused(NO_PACKAGE)),
                option(
                    "--skipinteg",
                    Kind::Kept(&["--skipchecksums", "--skippgpcheck"])
                ),
                option("-X", Kind::Refused(NO_SUCH_OPTION)),
                option("x", Kind::Refused(NO_SUCH_OPTION)),
            ]
        );
    }
}
