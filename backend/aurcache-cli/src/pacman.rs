//! What the local machine already installs from the AUR.
//!
//! A fresh AURCache does nothing until it is populated, and typing package
//! names one at a time is a poor first impression. `pacman -Qm` lists the
//! *foreign* packages — everything installed that no configured repository
//! carries, which on an Arch system is the AUR — and that is exactly the set a
//! new user wants their build server to mirror.

use anyhow::{Context, Result, bail};
use std::process::Command;

/// Package names out of `pacman -Qm` output.
///
/// Each line is `<name> <version>`. Only the name is kept: the version to build
/// is whatever upstream says now, not whatever this machine happens to have
/// installed.
#[must_use]
pub fn parse_foreign_packages(output: &str) -> Vec<String> {
    output
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .map(ToString::to_string)
        .collect()
}

/// Ask the local pacman which packages came from outside a repository.
pub fn installed_foreign_packages() -> Result<Vec<String>> {
    let output = Command::new("pacman").arg("-Qm").output().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            anyhow::anyhow!(
                "pacman not found on PATH; --from-installed only works on an Arch system"
            )
        } else {
            anyhow::Error::new(e).context("failed to run `pacman -Qm`")
        }
    })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("`pacman -Qm` failed: {}", stderr.trim());
    }

    let stdout = String::from_utf8(output.stdout).context("`pacman -Qm` printed invalid UTF-8")?;
    Ok(parse_foreign_packages(&stdout))
}

/// `wanted` first, then anything in `extra` not already present.
///
/// Order is the user's explicit list first because that is the one they typed;
/// duplicates are dropped rather than refused, since a package being both named
/// and installed is the expected case, not a mistake.
#[must_use]
pub fn merge_unique(wanted: Vec<String>, extra: Vec<String>) -> Vec<String> {
    // A set alongside the vec: `--from-installed` on a machine with hundreds
    // of foreign packages is quadratic over a `contains` scan. Owned keys —
    // borrowing the vec would forbid the pushes below.
    let mut seen: std::collections::HashSet<String> = wanted.iter().cloned().collect();
    let mut merged = wanted;
    for name in extra {
        if seen.insert(name.clone()) {
            merged.push(name);
        }
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::{merge_unique, parse_foreign_packages};

    #[test]
    fn the_name_is_taken_and_the_installed_version_dropped() {
        let listed = parse_foreign_packages("paru 2.0.4-1\nyay 12.4.2-1\n");
        assert_eq!(listed, vec!["paru", "yay"]);
    }

    #[test]
    fn blank_lines_and_stray_whitespace_are_ignored() {
        let listed = parse_foreign_packages("\n  paru 2.0.4-1  \n\n");
        assert_eq!(listed, vec!["paru"]);
    }

    #[test]
    fn no_foreign_packages_is_an_empty_list_not_an_error() {
        assert!(parse_foreign_packages("").is_empty());
    }

    /// Naming a package that is also installed is the expected case.
    #[test]
    fn a_package_named_and_installed_appears_once() {
        let merged = merge_unique(
            vec!["paru".to_string()],
            vec!["paru".to_string(), "yay".to_string()],
        );
        assert_eq!(merged, vec!["paru", "yay"]);
    }

    #[test]
    fn explicitly_named_packages_come_first() {
        let merged = merge_unique(vec!["zzz".to_string()], vec!["aaa".to_string()]);
        assert_eq!(merged, vec!["zzz", "aaa"]);
    }
}
