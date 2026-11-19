//! The dependencies a build installs before it starts.
//!
//! makepkg would install them itself, through `sudo pacman`, and so run
//! their `.install` scriptlets and alpm hooks as root with the build's
//! network. The build script installs them instead, as root before the
//! build, downloading with network and installing with none
//! (`aurcache-sandbox offline`); the build then runs with `no_new_privs`
//! and cannot become root at all. makepkg still checks its dependencies,
//! and fails the build if anything it needs is missing.
//!
//! The list is what makepkg would install: the package base's `depends`,
//! `makedepends` and, unless the build skips `check()`, `checkdepends`,
//! each with its arrays for the build's architecture, as written --
//! version constraints and provided names go to `pacman -S` the way makepkg
//! passes them. Under `--nodeps`, which has makepkg check none of them, it
//! is empty. It is read from `makepkg --printsrcinfo` of the PKGBUILD the
//! build runs, never from a shipped `.SRCINFO`, which a patch or a
//! maintainer's hand edit can leave describing another PKGBUILD.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// The `.SRCINFO` keys whose values makepkg installs before a build.
const RUNTIME: &str = "depends";
const BUILDTIME: &str = "makedepends";
const CHECKS: &str = "checkdepends";

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
        let flags = aurcache_common::build_flags::canonical(build_flags)
            .unwrap_or_else(|_| build_flags.to_vec());
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

    let mut deps: Vec<String> = Vec::new();
    for key in keys {
        let arch_key = format!("{key}_{arch}");
        for entry in &base {
            if (entry.key == key || entry.key == arch_key)
                && !entry.value.is_empty()
                && !deps.iter().any(|d| d == entry.value)
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

/// The `.SRCINFO` of the PKGBUILD in `pkgdir`, from `makepkg
/// --printsrcinfo` run as `build_user`.
///
/// Printing it sources the PKGBUILD, so it is confined exactly as the
/// build script's own host-side sourcing is: `aurcache-sandbox
/// --allow-build-env`, writing nothing but `pkgdir`, reading none of the
/// worker's secrets.
pub async fn print_srcinfo(pkgdir: &Path, build_user: &str) -> Result<String> {
    let output = tokio::process::Command::new("sudo")
        .args([
            "-u",
            build_user,
            "aurcache-sandbox",
            "--allow-build-env",
            "--",
        ])
        .args(["makepkg", "--printsrcinfo"])
        .current_dir(pkgdir)
        .kill_on_drop(true)
        .output()
        .await
        .context("running makepkg --printsrcinfo")?;
    if !output.status.success() {
        bail!(
            "makepkg --printsrcinfo failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    String::from_utf8(output.stdout).context("makepkg --printsrcinfo printed something not UTF-8")
}

/// Write `deps` one per line to `dir`, for the build script
/// (`AURCACHE_BUILD_DEPS`).
///
/// In the worker's own staging directory, which the build user cannot
/// write: the script reads it as root and hands each line to pacman.
pub fn stage(dir: &Path, deps: &[String]) -> Result<PathBuf> {
    let path = dir.join("build-deps");
    let mut text = String::new();
    for dep in deps {
        text.push_str(dep);
        text.push('\n');
    }
    std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
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

    #[test]
    fn staged_one_per_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = stage(dir.path(), &strings(&["glibc", "python>=3.11"])).unwrap();
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            "glibc\npython>=3.11\n"
        );
    }
}
