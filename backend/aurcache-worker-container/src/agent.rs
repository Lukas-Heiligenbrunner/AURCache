//! Where the agent binary is on the worker's own machine.
//!
//! The agent runs inside the builder image, for whatever architecture the
//! job is for, where the worker's own loader and libraries are not: it has
//! to be statically linked. That is not checked here. A dynamically linked
//! one fails every build as its container starts, with the engine's error.

use anyhow::{Result, bail};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// The variable naming the agent binary, for a layout the lookup does not
/// cover.
pub const VARIABLE: &str = "WORKER_BUILD_AGENT";
/// Where the worker's package installs the agent.
pub const INSTALLED: &str = "/usr/lib/aurcache/bin/aurcache-build-agent";

/// The agent binary on this machine: `WORKER_BUILD_AGENT`, else `INSTALLED`.
///
/// A variable that is set must name a file: pointing it somewhere that does
/// not exist is a mistake to report, not one to cover with another copy.
pub fn locate() -> Result<PathBuf> {
    find(std::env::var_os(VARIABLE), Path::new(INSTALLED))
}

/// The lookup over explicit places, so it can be tried without the process's
/// own environment.
fn find(variable: Option<OsString>, installed: &Path) -> Result<PathBuf> {
    if let Some(named) = variable.filter(|value| !value.is_empty()) {
        let named = PathBuf::from(named);
        if named.is_file() {
            return Ok(named);
        }
        bail!(
            "{VARIABLE} names {}, which is not a file (unset it to use {})",
            named.display(),
            installed.display()
        );
    }
    if installed.is_file() {
        return Ok(installed.to_path_buf());
    }
    bail!(
        "the build agent was not found: not named by {VARIABLE}, not at {}",
        installed.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn touch(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, b"").unwrap();
        path
    }

    #[test]
    fn the_variable_comes_first() {
        let dir = tempfile::tempdir().unwrap();
        let named = touch(dir.path(), "named");
        let installed = touch(dir.path(), "installed");
        let found = find(Some(named.clone().into()), &installed).unwrap();
        assert_eq!(found, named);
    }

    #[test]
    fn the_installed_path_is_used_when_nothing_is_named() {
        let dir = tempfile::tempdir().unwrap();
        let installed = touch(dir.path(), "installed");
        assert_eq!(find(None, &installed).unwrap(), installed);
        let empty = find(Some(OsString::new()), &installed).unwrap();
        assert_eq!(empty, installed, "an empty variable is not set");
    }

    #[test]
    fn a_missing_agent_names_every_place_looked() {
        let dir = tempfile::tempdir().unwrap();
        let installed = dir.path().join("installed");
        let error = find(None, &installed).unwrap_err().to_string();
        assert!(error.contains(VARIABLE), "{error}");
        assert!(error.contains(&installed.display().to_string()), "{error}");
    }

    #[test]
    fn a_variable_naming_nothing_is_an_error_not_a_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let installed = touch(dir.path(), "installed");
        let missing = dir.path().join("missing");
        let error = find(Some(missing.clone().into()), &installed)
            .unwrap_err()
            .to_string();
        assert!(error.contains(VARIABLE), "{error}");
        assert!(error.contains(&missing.display().to_string()), "{error}");
        assert!(error.contains(&installed.display().to_string()), "{error}");
    }
}
