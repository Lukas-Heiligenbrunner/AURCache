//! The dependencies a build installs before it starts, as every worker works
//! them out.
//!
//! makepkg would install them itself, as root, while the build runs. The
//! workers install them before the build instead, as root, and the build
//! then cannot become root at all; makepkg still checks its dependencies,
//! and fails the build if anything it needs is missing. Each worker reads
//! the list from `makepkg --printsrcinfo` of the PKGBUILD the build runs,
//! never from a shipped `.SRCINFO`, which a patch or a maintainer's hand
//! edit can leave describing another PKGBUILD; this is where that output
//! becomes the list, so the workers cannot disagree about it.
//!
//! The list is what makepkg would install: the package base's `depends`,
//! `makedepends` and, unless the build skips `check()`, `checkdepends`,
//! each with its arrays for the build's architecture, as written --
//! version constraints and provided names go to `pacman -S` the way makepkg
//! passes them. Under `--nodeps`, which has makepkg check none of them, it
//! is empty.

use std::collections::HashSet;

/// The `.SRCINFO` keys whose values makepkg installs before a build.
const RUNTIME: &str = "depends";
const BUILDTIME: &str = "makedepends";
const CHECKS: &str = "checkdepends";

/// The most of a `.SRCINFO` a worker reads: far more than any real one, and
/// a bound on what a PKGBUILD can make it hold.
pub const SRCINFO_MAX: u64 = 16 << 20;

/// What a build installs before it starts, given its makepkg flags.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Install {
    /// `--nodeps`: makepkg checks no dependency, so none is installed and
    /// the PKGBUILD is not read for them.
    Nothing,
    /// The PKGBUILD's dependencies.
    Dependencies(Checks),
}

/// Whether the build runs the PKGBUILD's `check()`, which decides whether
/// `checkdepends` are installed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Checks {
    Run,
    Skipped,
}

impl Install {
    /// What makepkg installs given these extra arguments, in any spelling
    /// it takes (`-d`, `-cd`, `--nodeps`).
    ///
    /// A `check()`-less PKGBUILD or a `!check` in `BUILDENV` also skips
    /// `check()`; installing its `checkdepends` anyway costs a download,
    /// where leaving one out would fail the build.
    #[must_use]
    pub fn of(build_flags: &[String]) -> Self {
        let flags =
            crate::build_flags::canonical(build_flags).unwrap_or_else(|_| build_flags.to_vec());
        let given = |flag: &str| flags.iter().any(|f| f == flag);
        if given("--nodeps") {
            Self::Nothing
        } else if given("--nocheck") {
            Self::Dependencies(Checks::Skipped)
        } else {
            Self::Dependencies(Checks::Run)
        }
    }
}

/// The dependencies `srcinfo` declares for a build on `arch`, in the order
/// makepkg installs them, each once.
///
/// Only the package base counts: makepkg installs what the PKGBUILD declares
/// globally, and a split package's own `depends` are its runtime metadata.
#[must_use]
pub fn from_srcinfo(srcinfo: &str, arch: &str, checks: Checks) -> Vec<String> {
    let mut keys = vec![RUNTIME, BUILDTIME];
    if checks == Checks::Run {
        keys.push(CHECKS);
    }
    let base: Vec<Entry> = srcinfo
        .lines()
        .skip_while(|line| !line.starts_with("pkgbase"))
        .skip(1)
        .take_while(|line| !line.starts_with("pkgname"))
        .filter_map(|line| line.trim().split_once('='))
        .map(|(key, value)| Entry {
            key: key.trim(),
            value: value.trim(),
        })
        .collect();

    let mut seen: HashSet<&str> = HashSet::new();
    let mut deps: Vec<String> = Vec::new();
    for key in keys {
        let arch_key = format!("{key}_{arch}");
        for entry in &base {
            if (entry.key == key || entry.key == arch_key)
                && !entry.value.is_empty()
                && seen.insert(entry.value)
            {
                deps.push(entry.value.to_string());
            }
        }
    }
    deps
}

/// One `key = value` line of a `.SRCINFO`.
struct Entry<'a> {
    key: &'a str,
    value: &'a str,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// As `makepkg --printsrcinfo` writes it.
    const SRCINFO: &str = "\
pkgbase = demo
\tpkgdesc = a demo
\tpkgver = 1.0
\tpkgrel = 1
\tarch = x86_64
\tarch = aarch64
\tcheckdepends = python-pytest
\tmakedepends = cmake
\tmakedepends = python>=3.11
\tmakedepends_aarch64 = arm-only-tool
\tdepends = glibc
\tdepends = sh
\tdepends_x86_64 = x86-only-lib>=2
\tdepends = cmake

pkgname = demo
\tdepends = split-only-dep

pkgname = demo-docs
\tdepends = docs-only-dep
";

    fn strings(list: &[&str]) -> Vec<String> {
        list.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn runtime_then_build_then_check_dependencies_as_written() {
        assert_eq!(
            from_srcinfo(SRCINFO, "x86_64", Checks::Run),
            strings(&[
                "glibc",
                "sh",
                "x86-only-lib>=2",
                "cmake",
                "python>=3.11",
                "python-pytest",
            ])
        );
    }

    #[test]
    fn only_the_builds_architecture_counts() {
        assert_eq!(
            from_srcinfo(SRCINFO, "aarch64", Checks::Run),
            strings(&[
                "glibc",
                "sh",
                "cmake",
                "python>=3.11",
                "arm-only-tool",
                "python-pytest",
            ])
        );
    }

    #[test]
    fn checkdepends_only_when_checks_run() {
        let deps = from_srcinfo(SRCINFO, "x86_64", Checks::Skipped);
        assert!(!deps.iter().any(|d| d == "python-pytest"), "{deps:?}");
        assert!(deps.iter().any(|d| d == "cmake"), "{deps:?}");
    }

    /// A split package's own dependencies are installed with it, not before
    /// the build, so makepkg leaves them out and so does this.
    #[test]
    fn a_split_packages_dependencies_are_not_the_builds() {
        let deps = from_srcinfo(SRCINFO, "x86_64", Checks::Run);
        assert!(!deps.iter().any(|d| d.contains("only-dep")), "{deps:?}");
    }

    #[test]
    fn a_package_with_no_dependencies_installs_nothing() {
        let srcinfo = "pkgbase = bare\n\tpkgver = 1\n\tarch = any\n\npkgname = bare\n";
        assert!(from_srcinfo(srcinfo, "x86_64", Checks::Run).is_empty());
        assert!(from_srcinfo("", "x86_64", Checks::Run).is_empty());
    }

    /// A key that merely starts like one of the three is not it.
    #[test]
    fn keys_match_exactly() {
        let srcinfo = "pkgbase = x\n\toptdepends = opt: why\n\tdepends_x86_64_v3 = v3\n";
        assert!(from_srcinfo(srcinfo, "x86_64", Checks::Run).is_empty());
    }

    /// The list is built in one pass, however long the `.SRCINFO`: a
    /// PKGBUILD can declare as many dependencies as it likes, and the
    /// worker works them out before any limit applies to the build.
    #[test]
    fn a_long_srcinfo_is_read_in_linear_time() {
        let mut srcinfo = String::from("pkgbase = big\n");
        for i in 0..200_000 {
            srcinfo.push_str(&format!("\tdepends = dep-{i}\n\tdepends = dep-{i}\n"));
        }
        let started = std::time::Instant::now();
        let deps = from_srcinfo(&srcinfo, "x86_64", Checks::Run);
        assert_eq!(deps.len(), 200_000);
        assert_eq!(deps.first().map(String::as_str), Some("dep-0"));
        assert_eq!(deps.last().map(String::as_str), Some("dep-199999"));
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn nocheck_skips_checks() {
        assert_eq!(
            Install::of(&strings(&["--nocheck"])),
            Install::Dependencies(Checks::Skipped)
        );
        assert_eq!(
            Install::of(&strings(&["--skippgpcheck"])),
            Install::Dependencies(Checks::Run)
        );
        assert_eq!(Install::of(&[]), Install::Dependencies(Checks::Run));
    }

    /// makepkg `-d` checks no dependency, so the build installs none, in
    /// whichever spelling it was given.
    #[test]
    fn nodeps_installs_nothing() {
        for flags in [
            &["--nodeps"][..],
            &["-d"],
            &["-cd"],
            &["--nocheck", "--nodeps"],
        ] {
            assert_eq!(Install::of(&strings(flags)), Install::Nothing, "{flags:?}");
        }
        assert_eq!(
            Install::of(&strings(&["--noconfirm", "-c"])),
            Install::Dependencies(Checks::Run)
        );
    }
}
