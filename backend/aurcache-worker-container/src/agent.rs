//! Where the agent binary is on the worker's own machine.
//!
//! The agent runs inside the builder image, for whatever architecture the
//! job is for, where the worker's own loader and libraries are not. So a
//! binary that asks for a loader is not one to hand a build: it is passed
//! over where another place may hold a static one, and refused where it was
//! named.

use anyhow::{Result, bail};
use aurcache_build_agent::layout::BIN_NAME;
use std::ffi::OsString;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
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
/// not exist is a mistake to report, not one to cover with another copy. A
/// binary beside the executable that is dynamically linked, as a plain
/// `cargo build` leaves one, is passed over for the installed one.
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
            if needs_loader(&named) {
                bail!("{VARIABLE} names {}, {DYNAMIC}", named.display());
            }
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
    let mut dynamic = None;
    for candidate in sibling.iter().map(PathBuf::as_path).chain([installed]) {
        if !candidate.is_file() {
            continue;
        }
        if !needs_loader(candidate) {
            return Ok(candidate.to_path_buf());
        }
        dynamic.get_or_insert(candidate);
    }
    if let Some(dynamic) = dynamic {
        bail!(
            "the build agent at {} {DYNAMIC}, and no other was found (name one with {VARIABLE})",
            dynamic.display()
        );
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

/// What is wrong with an agent that asks for a loader, after its path.
const DYNAMIC: &str = "is dynamically linked: builds run the agent inside the builder image, \
     where its loader and libraries are not. Build it with \
     RUSTFLAGS=\"-C target-feature=+crt-static\"";

/// The program header type of the one that names a loader.
const PT_INTERP: u32 = 3;

/// Whether the file at `path` is an ELF executable that asks for a dynamic
/// loader. No, for anything this cannot read as one: only what is known not
/// to run in another image is turned down.
fn needs_loader(path: &Path) -> bool {
    File::open(path)
        .ok()
        .and_then(|mut file| program_types(&mut file))
        .is_some_and(|types| types.contains(&PT_INTERP))
}

/// The types of an ELF file's program headers; `None` for a file that is not
/// ELF, or is cut short.
fn program_types(file: &mut (impl Read + Seek)) -> Option<Vec<u32>> {
    let mut header = [0u8; 64];
    file.read_exact(&mut header[..52]).ok()?;
    if header[..4] != *b"\x7fELF" {
        return None;
    }
    let wide = match header[4] {
        1 => false,
        2 => true,
        _ => return None,
    };
    let little = match header[5] {
        1 => true,
        2 => false,
        _ => return None,
    };
    if wide {
        file.read_exact(&mut header[52..]).ok()?;
    }
    let number = |bytes: &[u8]| {
        let mut padded = [0u8; 8];
        if little {
            padded[..bytes.len()].copy_from_slice(bytes);
            u64::from_le_bytes(padded)
        } else {
            padded[8 - bytes.len()..].copy_from_slice(bytes);
            u64::from_be_bytes(padded)
        }
    };
    // Where the program headers start, how long each is, how many there are.
    let (start, size, count) = if wide {
        (
            number(&header[32..40]),
            number(&header[54..56]),
            number(&header[56..58]),
        )
    } else {
        (
            number(&header[28..32]),
            number(&header[42..44]),
            number(&header[44..46]),
        )
    };
    let mut types = Vec::new();
    for index in 0..count {
        file.seek(SeekFrom::Start(
            start.checked_add(index.checked_mul(size)?)?,
        ))
        .ok()?;
        let mut kind = [0u8; 4];
        file.read_exact(&mut kind).ok()?;
        types.push(u32::try_from(number(&kind)).ok()?);
    }
    Some(types)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A 64-bit little-endian ELF file of nothing but its header and program
    /// headers of the given types.
    fn elf(types: &[u32]) -> Vec<u8> {
        let mut file = vec![0u8; 64];
        file[..6].copy_from_slice(b"\x7fELF\x02\x01");
        file[32..40].copy_from_slice(&64u64.to_le_bytes());
        file[54..56].copy_from_slice(&56u16.to_le_bytes());
        file[56..58].copy_from_slice(&u16::try_from(types.len()).unwrap().to_le_bytes());
        for kind in types {
            let mut header = [0u8; 56];
            header[..4].copy_from_slice(&kind.to_le_bytes());
            file.extend_from_slice(&header);
        }
        file
    }

    fn write(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, bytes).unwrap();
        path
    }

    const LOAD: u32 = 1;

    #[test]
    fn a_program_that_names_a_loader_is_told_from_one_that_does_not() {
        let dir = tempfile::tempdir().unwrap();
        let dynamic = write(dir.path(), "dynamic", &elf(&[LOAD, PT_INTERP, LOAD]));
        let fixed = write(dir.path(), "static", &elf(&[LOAD, LOAD]));
        let script = write(dir.path(), "script", b"#!/bin/sh\n");
        let cut = write(dir.path(), "cut", &elf(&[LOAD, PT_INTERP])[..70]);
        assert!(needs_loader(&dynamic));
        assert!(!needs_loader(&fixed));
        assert!(!needs_loader(&script));
        assert!(!needs_loader(&cut));
        assert!(!needs_loader(&dir.path().join("missing")));
    }

    /// The binary a plain `cargo build` leaves beside the worker does not
    /// win over the installed, static one.
    #[test]
    fn a_dynamic_sibling_is_passed_over_for_the_installed_agent() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), BIN_NAME, &elf(&[LOAD, PT_INTERP]));
        let installed = write(dir.path(), "installed", &elf(&[LOAD]));
        assert_eq!(find(None, Some(dir.path()), &installed).unwrap(), installed);
    }

    #[test]
    fn an_agent_that_is_only_dynamic_is_refused_saying_how_to_build_one() {
        let dir = tempfile::tempdir().unwrap();
        let sibling = write(dir.path(), BIN_NAME, &elf(&[LOAD, PT_INTERP]));
        let installed = dir.path().join("installed");
        let error = find(None, Some(dir.path()), &installed)
            .unwrap_err()
            .to_string();
        assert!(error.contains(&sibling.display().to_string()), "{error}");
        assert!(error.contains("dynamically linked"), "{error}");
        assert!(error.contains("crt-static"), "{error}");

        let named = find(Some(sibling.into()), None, &installed)
            .unwrap_err()
            .to_string();
        assert!(named.contains(VARIABLE), "{named}");
        assert!(named.contains("dynamically linked"), "{named}");
    }

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
