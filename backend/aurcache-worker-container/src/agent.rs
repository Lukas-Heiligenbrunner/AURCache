//! Where the agent binary is on the worker's own machine.

use anyhow::{Result, bail};
use aurcache_build_agent::layout::BIN_NAME;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// The variable naming the agent binary, for a layout the lookup does not
/// cover.
pub const VARIABLE: &str = "WORKER_BUILD_AGENT";
/// Where the worker's package installs the agent.
pub const INSTALLED: &str = "/usr/lib/aurcache/bin/aurcache-build-agent";

/// The agent binary on this machine: `WORKER_BUILD_AGENT`, else beside the
/// running executable, else `INSTALLED`.
///
/// A variable that is set must name a file: pointing it somewhere that does
/// not exist is a mistake to report, not one to cover with another copy.
pub fn locate() -> Result<PathBuf> {
    let beside = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf));
    find(
        std::env::var_os(VARIABLE),
        beside.as_deref(),
        Path::new(INSTALLED),
    )
}

/// The lookup over explicit places, so it can be tried without the process's
/// own environment.
fn find(variable: Option<OsString>, beside: Option<&Path>, installed: &Path) -> Result<PathBuf> {
    if let Some(named) = variable.filter(|value| !value.is_empty()) {
        let named = PathBuf::from(named);
        if named.is_file() {
            return Ok(named);
        }
        bail!(
            "{VARIABLE} names {}, which is not a file (unset it to look beside the worker's \
             executable, then at {})",
            named.display(),
            installed.display()
        );
    }
    let sibling = beside.map(|dir| dir.join(BIN_NAME));
    if let Some(sibling) = sibling.as_ref().filter(|path| path.is_file()) {
        return Ok(sibling.clone());
    }
    if installed.is_file() {
        return Ok(installed.to_path_buf());
    }
    let sibling = sibling.map_or_else(
        || format!("beside the worker's executable (whose location is unknown) as {BIN_NAME}"),
        |path| format!("beside the worker's executable at {}", path.display()),
    );
    bail!(
        "the build agent was not found: not named by {VARIABLE}, not {sibling}, not at {}",
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
        touch(dir.path(), BIN_NAME);
        let installed = touch(dir.path(), "installed");
        let found = find(Some(named.clone().into()), Some(dir.path()), &installed).unwrap();
        assert_eq!(found, named);
    }

    #[test]
    fn the_sibling_comes_before_the_installed_path() {
        let dir = tempfile::tempdir().unwrap();
        let sibling = touch(dir.path(), BIN_NAME);
        let installed = touch(dir.path(), "installed");
        let found = find(None, Some(dir.path()), &installed).unwrap();
        assert_eq!(found, sibling);
        let empty = find(Some(OsString::new()), Some(dir.path()), &installed).unwrap();
        assert_eq!(empty, sibling, "an empty variable is not set");
    }

    #[test]
    fn the_installed_path_is_the_last_resort() {
        let dir = tempfile::tempdir().unwrap();
        let installed = touch(dir.path(), "installed");
        let beside = dir.path().join("elsewhere");
        assert_eq!(find(None, Some(&beside), &installed).unwrap(), installed);
        assert_eq!(find(None, None, &installed).unwrap(), installed);
    }

    #[test]
    fn a_missing_agent_names_every_place_looked() {
        let dir = tempfile::tempdir().unwrap();
        let installed = dir.path().join("installed");
        let error = find(None, Some(dir.path()), &installed)
            .unwrap_err()
            .to_string();
        assert!(error.contains(VARIABLE), "{error}");
        assert!(error.contains(BIN_NAME), "{error}");
        assert!(error.contains(&installed.display().to_string()), "{error}");
    }

    #[test]
    fn a_variable_naming_nothing_is_an_error_not_a_fallback() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), BIN_NAME);
        let missing = dir.path().join("missing");
        let error = find(
            Some(missing.clone().into()),
            Some(dir.path()),
            Path::new(INSTALLED),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains(VARIABLE), "{error}");
        assert!(error.contains(&missing.display().to_string()), "{error}");
        assert!(error.contains(INSTALLED), "{error}");
    }
}
