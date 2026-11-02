//! Canonical filesystem locations for pacman mirrorlists.
//!
//! Everything that reads or writes a mirrorlist resolves its path through here,
//! so the writers and readers cannot drift apart. They previously did: startup
//! wrote `./repo/mirrorlist` while official-repo dependency resolution read
//! `./config/pacman_x86_64/mirrorlist`, which made every package with
//! dependencies fail to add.
//!
//! These live in this crate rather than `aurcache-utils` because
//! `aurcache-db`'s dependency-backfill migration constructs an [`AurClient`],
//! so `aurcache-deps` cannot depend on `aurcache-utils` without a cycle.
//! `aurcache_utils::job_config` re-exports them so the rest of the workspace
//! has one obvious import.
//!
//! [`AurClient`]: crate::AurClient
//!
//! # Layout
//!
//! Mirrorlists are keyed by architecture: `<dir>/mirrorlist.<arch>`. Only
//! `x86_64` is populated today — it is the only architecture AURCache can rank
//! mirrors for — but the layout is arch-keyed from the start so adding one later
//! means writing another file rather than reworking every path.
//!
//! A single plain `<dir>/mirrorlist` is a **convenience mount**: it is adopted
//! into the native architecture's slot at startup (see `aurcache::startup`),
//! which lets an operator mount one file without caring about the arch suffix.

use std::path::PathBuf;

/// Directory AURCache stores and serves mirrorlists from.
/// Override with `AURCACHE_MIRRORLIST_DIR` (default `./repo`).
#[must_use]
pub fn mirrorlist_dir() -> PathBuf {
    PathBuf::from(std::env::var("AURCACHE_MIRRORLIST_DIR").unwrap_or_else(|_| "./repo".to_string()))
}

/// Mirrorlist for one architecture: `<dir>/mirrorlist.<arch>`.
#[must_use]
pub fn mirrorlist_path(arch: &str) -> PathBuf {
    mirrorlist_dir().join(mirrorlist_file_name(arch))
}

/// File name for an architecture's mirrorlist, relative to the mirrorlist dir.
#[must_use]
pub fn mirrorlist_file_name(arch: &str) -> String {
    format!("mirrorlist.{arch}")
}

/// The arch-agnostic mirrorlist an operator may mount for convenience.
/// Adopted into [`mirrorlist_path`] for the native architecture at startup;
/// nothing reads it after that.
#[must_use]
pub fn shared_mirrorlist_path() -> PathBuf {
    mirrorlist_dir().join("mirrorlist")
}

/// Where 0.5.0 read a mirrorlist an operator mounted: `mirrorlist` in
/// `MIRRORLIST_PATH_X86_64`, by default `./config/pacman_x86_64`. `None` in
/// its host build mode (`BUILD_ARTIFACT_DIR` set), where 0.5.0 wrote one
/// there itself, so a file there says nothing about who chose its mirrors.
#[must_use]
pub fn legacy_mirrorlist_path() -> Option<PathBuf> {
    if std::env::var_os("BUILD_ARTIFACT_DIR").is_some() {
        return None;
    }
    let dir = std::env::var("MIRRORLIST_PATH_X86_64")
        .unwrap_or_else(|_| "./config/pacman_x86_64".to_string());
    Some(PathBuf::from(dir).join("mirrorlist"))
}

/// The mirrorlist the operator mounted, if there is one: the
/// [`shared_mirrorlist_path`], or where 0.5.0 read one, so an upgraded
/// instance keeps the mirrors its operator chose. In a container a file
/// survives in either place only if it was mounted there.
#[must_use]
pub fn mounted_mirrorlist() -> Option<PathBuf> {
    std::iter::once(shared_mirrorlist_path())
        .chain(legacy_mirrorlist_path())
        .find(|path| path.is_file())
}

/// The architecture AURCache itself is running on, in Arch Linux's naming.
///
/// Rust and Arch disagree on 32-bit ARM (`arm` vs `armv7h`); everything else we
/// support matches. An unrecognised target falls through unmapped, which at
/// worst means a mounted mirrorlist is adopted under an unexpected name rather
/// than silently discarded.
#[must_use]
pub fn native_arch() -> &'static str {
    match std::env::consts::ARCH {
        "arm" => "armv7h",
        other => other,
    }
}

/// Where downloaded official repo databases (`core.db` etc.) are cached.
/// Override with `OFFICIAL_REPO_CACHE_DIR`.
///
/// Defaults inside the mirrorlist dir so it lands in the same persisted volume
/// as the repo, rather than being re-downloaded on every boot.
#[must_use]
pub fn official_repo_cache_dir() -> PathBuf {
    if let Ok(path) = std::env::var("OFFICIAL_REPO_CACHE_DIR") {
        return PathBuf::from(path);
    }
    mirrorlist_dir().join("official_repo_cache")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mirrorlists_are_keyed_by_architecture() {
        assert_eq!(mirrorlist_file_name("x86_64"), "mirrorlist.x86_64");
        assert_eq!(mirrorlist_file_name("aarch64"), "mirrorlist.aarch64");
        // The convenience mount is deliberately *not* one of them.
        assert_ne!(
            shared_mirrorlist_path().file_name().unwrap(),
            mirrorlist_path("x86_64").file_name().unwrap()
        );
    }

    #[test]
    fn arch_specific_and_shared_paths_share_a_directory() {
        let shared = shared_mirrorlist_path();
        let arch = mirrorlist_path("x86_64");
        assert_eq!(shared.parent(), arch.parent());
    }

    #[test]
    fn native_arch_uses_arch_linux_naming() {
        // Whatever the host, the mapping must never yield Rust's `arm` spelling,
        // which is not what Arch calls it.
        assert_ne!(native_arch(), "arm");
    }
}
