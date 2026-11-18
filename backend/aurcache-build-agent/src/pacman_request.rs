//! What the pacman helper accepts from makepkg, and what root runs for it.
//!
//! makepkg installs a build's missing dependencies through `PACMAN_AUTH`,
//! which the agent points at its own `pacman-request` subcommand. Root accepts
//! `pacman [--noconfirm|--noprogressbar|--needed|--color X] -S --asdeps
//! <targets>` and nothing else, and runs a command line it builds from the
//! validated targets alone. The grammar is in `container-workers.md`
//! ("The pacman helper").

use std::fmt;
use std::path::Path;

/// What root will install: validated targets, nothing of the caller's argv.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Install {
    pub targets: Vec<String>,
}

/// Why a request was refused: one variant per reason, whose `Display` names
/// the offending word.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// No word was sent at all.
    Empty,
    /// The first word is not pacman's path.
    NotPacman { word: String },
    /// An operation other than `-S` on its own.
    Operation { word: String },
    /// A word before the operation that is not one of the options makepkg
    /// passes on.
    Option { word: String },
    /// `--color` followed by something other than its three values.
    Color { word: String },
    /// The words end before `-S --asdeps`.
    Incomplete,
    /// `-S` is followed by something other than `--asdeps`.
    NotAsDeps { word: String },
    /// `-S --asdeps` names no package.
    NoTargets,
    /// A word starting with a dash where a package belongs.
    OptionAsTarget { word: String },
    /// An empty word where a package belongs.
    EmptyTarget,
    /// A target that is not a package name: a path, `repo/name`, anything
    /// with a character no package name has.
    Name { word: String },
    /// A target whose version constraint is not one pacman hands back.
    Version { word: String },
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "nothing to run"),
            Self::NotPacman { word } => write!(f, "`{word}` is not pacman"),
            Self::Operation { word } => {
                write!(
                    f,
                    "`{word}` is not installing dependencies: only `-S --asdeps` is run"
                )
            }
            Self::Option { word } => write!(f, "`{word}` is not an option that is passed on"),
            Self::Color { word } => write!(f, "`{word}` is not a value of --color"),
            Self::Incomplete => write!(f, "the command ends before `-S --asdeps`"),
            Self::NotAsDeps { word } => {
                write!(f, "`{word}` follows -S where only `--asdeps` may")
            }
            Self::NoTargets => write!(f, "no package was named"),
            Self::OptionAsTarget { word } => {
                write!(f, "`{word}` is an option where a package name belongs")
            }
            Self::EmptyTarget => write!(f, "an empty word where a package name belongs"),
            Self::Name { word } => write!(f, "`{word}` is not a package name"),
            Self::Version { word } => write!(f, "`{word}` has no valid version constraint"),
        }
    }
}

impl std::error::Error for Refusal {}

/// `argv` is what makepkg ran after `PACMAN_AUTH`: pacman's path first.
pub fn validate(argv: &[String]) -> Result<Install, Refusal> {
    let mut words = argv.iter();
    let pacman = words.next().ok_or(Refusal::Empty)?;
    if !names_pacman(pacman) {
        return Err(Refusal::NotPacman {
            word: pacman.clone(),
        });
    }
    loop {
        let word = words.next().ok_or(Refusal::Incomplete)?;
        match word.as_str() {
            "--noconfirm" | "--noprogressbar" | "--needed" => {}
            "--color" => {
                let value = words.next().ok_or(Refusal::Incomplete)?;
                if !matches!(value.as_str(), "never" | "auto" | "always") {
                    return Err(Refusal::Color {
                        word: value.clone(),
                    });
                }
            }
            "-S" => break,
            _ if is_operation(word) => return Err(Refusal::Operation { word: word.clone() }),
            _ => return Err(Refusal::Option { word: word.clone() }),
        }
    }
    let asdeps = words.next().ok_or(Refusal::Incomplete)?;
    if asdeps != "--asdeps" {
        return Err(Refusal::NotAsDeps {
            word: asdeps.clone(),
        });
    }
    let targets = words
        .map(|word| target(word).map(|()| word.clone()))
        .collect::<Result<Vec<_>, _>>()?;
    if targets.is_empty() {
        return Err(Refusal::NoTargets);
    }
    Ok(Install { targets })
}

/// The command line root runs for it.
///
/// Every word but the targets is the helper's own. `--` ends pacman's option
/// parsing, so a target is a target whatever it looks like.
#[must_use]
pub fn command(install: &Install) -> Vec<String> {
    const FIXED: [&str; 8] = [
        "pacman",
        "--noconfirm",
        "--noprogressbar",
        "--color",
        "never",
        "-S",
        "--asdeps",
        "--",
    ];
    FIXED
        .iter()
        .map(ToString::to_string)
        .chain(install.targets.iter().cloned())
        .collect()
}

/// Whether makepkg's first word is pacman: its name, under any directory.
/// The word is never run, so which directory does not matter.
fn names_pacman(word: &str) -> bool {
    Path::new(word).file_name().is_some_and(|n| n == "pacman")
}

/// Whether a word is one of pacman's operations, alone or with flags folded
/// into it (`-U`, `-Rnu`, `-Sy`).
fn is_operation(word: &str) -> bool {
    let mut chars = word.chars();
    chars.next() == Some('-') && chars.next().is_some_and(|c| c.is_ascii_uppercase())
}

/// Accept one target: a package name, optionally with a version constraint.
fn target(word: &str) -> Result<(), Refusal> {
    if word.is_empty() {
        return Err(Refusal::EmptyTarget);
    }
    if word.starts_with('-') {
        return Err(Refusal::OptionAsTarget {
            word: word.to_string(),
        });
    }
    let (name, constraint) = match word.find(['<', '=', '>']) {
        Some(at) => (&word[..at], Some(&word[at..])),
        None => (word, None),
    };
    if !is_name(name) {
        return Err(Refusal::Name {
            word: word.to_string(),
        });
    }
    if constraint.is_some_and(|c| !is_constraint(c)) {
        return Err(Refusal::Version {
            word: word.to_string(),
        });
    }
    Ok(())
}

/// `[A-Za-z0-9@_+][A-Za-z0-9@._+-]*`
fn is_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric() || matches!(c, '@' | '_' | '+'))
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '@' | '.' | '_' | '+' | '-'))
}

/// One of `<`, `<=`, `=`, `>=`, `>`, then a version of `[A-Za-z0-9._+:~-]+`.
fn is_constraint(constraint: &str) -> bool {
    let version = ["<=", ">=", "<", "=", ">"]
        .iter()
        .find_map(|op| constraint.strip_prefix(op));
    version.is_some_and(|v| {
        !v.is_empty()
            && v.chars().all(|c| {
                c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | ':' | '~' | '-')
            })
    })
}

#[cfg(test)]
mod tests {
    use super::{Install, Refusal, command, validate};

    const PACMAN: &str = "/usr/bin/pacman";

    fn words(line: &[&str]) -> Vec<String> {
        line.iter().map(ToString::to_string).collect()
    }

    /// What makepkg runs after `PACMAN_AUTH`: pacman's path, then `rest`.
    fn sent(rest: &[&str]) -> Vec<String> {
        std::iter::once(PACMAN)
            .chain(rest.iter().copied())
            .map(ToString::to_string)
            .collect()
    }

    /// What makepkg really sends: `PACMAN_PATH`, whichever of its
    /// `PACMAN_OPTS` words it was started with, `-S --asdeps`, and what
    /// `pacman -T` said was missing.
    #[test]
    fn what_makepkg_sends_is_accepted() {
        let accepted: &[&[&str]] = &[
            &["-S", "--asdeps", "tree"],
            // With each word of `PACMAN_OPTS`, alone and together.
            &["--noconfirm", "-S", "--asdeps", "tree"],
            &["--noprogressbar", "-S", "--asdeps", "tree"],
            &["--needed", "-S", "--asdeps", "tree"],
            &["--color", "never", "-S", "--asdeps", "tree"],
            &["--color", "always", "-S", "--asdeps", "tree"],
            &["--color", "auto", "-S", "--asdeps", "tree"],
            &[
                "--noconfirm",
                "--noprogressbar",
                "--color",
                "never",
                "-S",
                "--asdeps",
                "cmake",
                "ninja",
            ],
            // Versioned targets, as `pacman -T` hands them back.
            &["-S", "--asdeps", "glibc>=2.38"],
            &["-S", "--asdeps", "qt6-base=6.8.0-1"],
            &["-S", "--asdeps", "java-runtime<22"],
            &["-S", "--asdeps", "foo<=1:2.0~rc1+3"],
            &["-S", "--asdeps", "bar>1.0_2"],
            // Names with every character a package name has.
            &["-S", "--asdeps", "libfoo.so=1-64"],
            &["-S", "--asdeps", "gtk+", "@scope", "_x"],
            &["-S", "--asdeps", "lib32-gcc-libs", "r8168-dkms"],
            // A split package's members ask for all their dependencies at once.
            &[
                "--noconfirm",
                "-S",
                "--asdeps",
                "python",
                "python-build",
                "python-installer>=0.7",
                "python-wheel",
            ],
        ];
        for rest in accepted {
            let install = validate(&sent(rest)).unwrap_or_else(|e| panic!("{rest:?} refused: {e}"));
            let at = rest.iter().position(|w| *w == "--asdeps").unwrap();
            assert_eq!(install.targets, words(&rest[at + 1..]), "{rest:?}");
        }
    }

    /// The first word is pacman wherever makepkg found it: it is never run.
    #[test]
    fn pacman_is_pacman_under_any_directory() {
        for pacman in [
            "pacman",
            "/usr/bin/pacman",
            "/usr/sbin/pacman",
            "/bin/pacman",
        ] {
            let argv = words(&[pacman, "-S", "--asdeps", "tree"]);
            assert!(validate(&argv).is_ok(), "{pacman}");
        }
        for other in ["/usr/bin/sh", "-S", "/usr/bin/pacman-key", "pacman.sh", ""] {
            let argv = words(&[other, "-S", "--asdeps", "tree"]);
            let expected = Refusal::NotPacman { word: other.into() };
            assert_eq!(validate(&argv), Err(expected), "{other:?}");
        }
        assert_eq!(validate(&[]), Err(Refusal::Empty));
    }

    #[test]
    fn everything_else_is_refused() {
        let operation = |word: &str| Refusal::Operation { word: word.into() };
        let option = |word: &str| Refusal::Option { word: word.into() };
        let color = |word: &str| Refusal::Color { word: word.into() };
        let not_asdeps = |word: &str| Refusal::NotAsDeps { word: word.into() };
        let dashed = |word: &str| Refusal::OptionAsTarget { word: word.into() };
        let name = |word: &str| Refusal::Name { word: word.into() };
        let version = |word: &str| Refusal::Version { word: word.into() };

        let refused: &[(&[&str], Refusal)] = &[
            // Other operations, and `-S` with anything folded into it.
            (
                &["-U", "/work/pkg/evil-1-1-any.pkg.tar.zst"],
                operation("-U"),
            ),
            (&["--noconfirm", "-U", "evil.pkg.tar.zst"], operation("-U")),
            (&["--noconfirm", "-Rnu", "tree"], operation("-Rnu")),
            (&["-Sy", "--asdeps", "tree"], operation("-Sy")),
            (&["-Syu"], operation("-Syu")),
            (&["-Sw", "--asdeps", "tree"], operation("-Sw")),
            (&["-Q"], operation("-Q")),
            // `-S` without `--asdeps` right after it.
            (&["-S", "tree"], not_asdeps("tree")),
            (&["-S", "--asexplicit", "tree"], not_asdeps("--asexplicit")),
            (
                &["-S", "--noconfirm", "--asdeps", "tree"],
                not_asdeps("--noconfirm"),
            ),
            (&["-S", "-S", "--asdeps", "tree"], not_asdeps("-S")),
            // Options makepkg never sends, before the operation.
            (&["--asdeps", "-S", "tree"], option("--asdeps")),
            (
                &["--config", "/tmp/c", "-S", "--asdeps", "tree"],
                option("--config"),
            ),
            (
                &["--root", "/tmp/r", "-S", "--asdeps", "tree"],
                option("--root"),
            ),
            (&["-r", "/tmp/r", "-S", "--asdeps", "tree"], option("-r")),
            (
                &["--dbpath=/tmp/db", "-S", "--asdeps", "tree"],
                option("--dbpath=/tmp/db"),
            ),
            (
                &["--sysroot", "/tmp/r", "-S", "--asdeps", "tree"],
                option("--sysroot"),
            ),
            (&["--sync", "--asdeps", "tree"], option("--sync")),
            (&["--", "-S", "--asdeps", "tree"], option("--")),
            (&["tree", "-S", "--asdeps", "tree"], option("tree")),
            (&["", "-S", "--asdeps", "tree"], option("")),
            (&["--color", "-S", "--asdeps", "tree"], color("-S")),
            (&["--color", "red", "-S", "--asdeps", "tree"], color("red")),
            // After `--asdeps`, nothing that starts with a dash.
            (
                &["-S", "--asdeps", "--overwrite", "*", "tree"],
                dashed("--overwrite"),
            ),
            (
                &["-S", "--asdeps", "tree", "--hookdir", "/tmp/h"],
                dashed("--hookdir"),
            ),
            (
                &["-S", "--asdeps", "tree", "--dbpath", "/tmp/db"],
                dashed("--dbpath"),
            ),
            (
                &["-S", "--asdeps", "--config=/tmp/c", "tree"],
                dashed("--config=/tmp/c"),
            ),
            (&["-S", "--asdeps", "--needed", "tree"], dashed("--needed")),
            (&["-S", "--asdeps", "--", "tree"], dashed("--")),
            (&["-S", "--asdeps", "-"], dashed("-")),
            (&["-S", "--asdeps", "-y"], dashed("-y")),
            // Not package names: paths, repositories, URLs, shell.
            (
                &["-S", "--asdeps", "/work/pkg/x.pkg.tar.zst"],
                name("/work/pkg/x.pkg.tar.zst"),
            ),
            (
                &["-S", "--asdeps", "./x.pkg.tar.zst"],
                name("./x.pkg.tar.zst"),
            ),
            (&["-S", "--asdeps", "extra/tree"], name("extra/tree")),
            (
                &["-S", "--asdeps", "file:///x.pkg.tar.zst"],
                name("file:///x.pkg.tar.zst"),
            ),
            (&["-S", "--asdeps", ".hidden"], name(".hidden")),
            (&["-S", "--asdeps", "tree;id"], name("tree;id")),
            (&["-S", "--asdeps", "$(id)"], name("$(id)")),
            (&["-S", "--asdeps", "`id`"], name("`id`")),
            (&["-S", "--asdeps", "tree id"], name("tree id")),
            (&["-S", "--asdeps", "tree\nid"], name("tree\nid")),
            (&["-S", "--asdeps", "tree*"], name("tree*")),
            (&["-S", "--asdeps", "tr\u{e9}e"], name("tr\u{e9}e")),
            (&["-S", "--asdeps", ">=1"], name(">=1")),
            (&["-S", "--asdeps", "tree", ""], Refusal::EmptyTarget),
            // Not version constraints.
            (&["-S", "--asdeps", "tree>="], version("tree>=")),
            (
                &["-S", "--asdeps", "tree=1 --overwrite"],
                version("tree=1 --overwrite"),
            ),
            (&["-S", "--asdeps", "tree==1"], version("tree==1")),
            (&["-S", "--asdeps", "tree=>1"], version("tree=>1")),
            (&["-S", "--asdeps", "tree>=1/../x"], version("tree>=1/../x")),
            (&["-S", "--asdeps", "tree=1;id"], version("tree=1;id")),
            // No target at all, and words that end early.
            (&["-S", "--asdeps"], Refusal::NoTargets),
            (&["-S"], Refusal::Incomplete),
            (&[], Refusal::Incomplete),
            (&["--noconfirm"], Refusal::Incomplete),
            (&["--color"], Refusal::Incomplete),
            (&["--color", "never"], Refusal::Incomplete),
        ];
        for (rest, reason) in refused {
            assert_eq!(validate(&sent(rest)).as_ref(), Err(reason), "{rest:?}");
        }
    }

    /// Each refusal says which word it is about.
    #[test]
    fn a_refusal_names_the_offending_word() {
        let cases: &[(&[&str], &str)] = &[
            (&["-U", "x"], "`-U`"),
            (&["--config", "x", "-S", "--asdeps", "tree"], "`--config`"),
            (&["--color", "red", "-S", "--asdeps", "tree"], "`red`"),
            (&["-S", "tree"], "`tree`"),
            (&["-S", "--asdeps", "--overwrite"], "`--overwrite`"),
            (&["-S", "--asdeps", "extra/tree"], "`extra/tree`"),
            (&["-S", "--asdeps", "tree=="], "`tree==`"),
        ];
        for (rest, word) in cases {
            let said = validate(&sent(rest)).unwrap_err().to_string();
            assert!(said.contains(word), "{said}");
        }
        let said = validate(&words(&["sh", "-c", "id"]))
            .unwrap_err()
            .to_string();
        assert!(said.contains("`sh`"), "{said}");
    }

    /// Root's command line is its own words and the validated targets:
    /// nothing else of what was sent reaches it.
    #[test]
    fn the_command_holds_no_word_of_the_request_but_its_targets() {
        let argv = words(&[
            "/somewhere/odd/pacman",
            "--needed",
            "--color",
            "always",
            "--noprogressbar",
            "-S",
            "--asdeps",
            "tree",
            "glibc>=2.38",
        ]);
        let command = command(&validate(&argv).unwrap());
        assert_eq!(
            command,
            [
                "pacman",
                "--noconfirm",
                "--noprogressbar",
                "--color",
                "never",
                "-S",
                "--asdeps",
                "--",
                "tree",
                "glibc>=2.38"
            ]
        );
        for word in ["/somewhere/odd/pacman", "--needed", "always"] {
            assert!(!command.iter().any(|w| w == word), "{word} was passed on");
        }
    }

    /// The command is the same whatever options came with the request.
    #[test]
    fn the_command_depends_on_the_targets_alone() {
        let install = Install {
            targets: vec!["tree".to_string()],
        };
        let bare = validate(&sent(&["-S", "--asdeps", "tree"])).unwrap();
        let full = validate(&sent(&[
            "--noconfirm",
            "--needed",
            "-S",
            "--asdeps",
            "tree",
        ]))
        .unwrap();
        assert_eq!(bare, install);
        assert_eq!(full, install);
        assert_eq!(command(&bare), command(&full));
        assert_eq!(command(&install).last().map(String::as_str), Some("tree"));
    }
}
