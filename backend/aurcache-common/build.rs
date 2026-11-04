//! Bake the commit this binary was built from into the build.
//!
//! The version a binary reports is its crate release plus, when the build is
//! not exactly that release, the commit it came from (see `src/version.rs`).
//! That decision needs three facts at compile time, resolved here in order:
//!
//! 1. Explicit overrides, for builds without a checkout. Container images
//!    exclude `.git` (see `.dockerignore`), so the image builds pass the
//!    commit in as `LATEST_COMMIT_SHA` — the variable the server's startup
//!    banner already read — and, for tag builds, the tag as
//!    `AURCACHE_GIT_TAG`. `AURCACHE_GIT_DIRTY=1` says that tree was dirty;
//!    when it is unset the checkout is asked instead.
//! 2. The checkout itself, for developer and CI builds.
//! 3. Nothing, for tarballs without git metadata (crates.io, the tag
//!    tarballs the AUR packages build from): the reported version is then
//!    the bare crate release, which is exactly right for a release artifact.
//!
//! Git is never let above the source tree to look for a repository. A
//! tarball unpacked inside another checkout -- `makepkg` extracts into the
//! AUR clone the PKGBUILD came from -- would otherwise report that clone's
//! commit as its own.
//!
//! Nothing here may fail the build: git missing, failing, or slow is not an
//! error, it just means there is less to report.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The top of the source tree this crate was built from: the repository
/// root in the tree's own layout, else the crate's own directory.
fn source_root() -> Option<PathBuf> {
    let manifest = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR")?);
    let tree = manifest.ancestors().nth(2)?;
    let root = if tree.join("backend/aurcache-common/build.rs").is_file() {
        tree
    } else {
        &manifest
    };
    root.canonicalize().ok()
}

/// One git probe, never failing the build.
fn git(args: &[&str]) -> Option<String> {
    let mut command = Command::new("git");
    // Nothing above the source tree: a repository found there is someone
    // else's. Without a root to bound the search, there is no probe at all.
    let ceiling = source_root()?.parent()?.to_path_buf();
    command
        .env("GIT_CEILING_DIRECTORIES", ceiling)
        // Never take the index lock: this is a read-only question asked on
        // every build, including ones running beside an editor or a fetch.
        .env("GIT_OPTIONAL_LOCKS", "0")
        .args(args)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|stdout| stdout.trim().to_string())
        .filter(|stdout| !stdout.is_empty())
}

/// The checkout's git directory, when there is one.
fn git_dir() -> Option<PathBuf> {
    git(&["rev-parse", "--absolute-git-dir"]).map(PathBuf::from)
}

/// Watch a file for rebuilds, when it exists. A missing path (a packed ref
/// that was never unpacked, a checkout without the file) is not an error.
fn watch(path: &Path) {
    if path.is_file() {
        println!("cargo:rerun-if-changed={}", path.display());
    }
}

/// The commit override, if one names a real commit.
///
/// `dev` is the Dockerfiles' default when no SHA is passed in, and names
/// nothing: treating it as a commit would report `+gdev` on images whose
/// commit was never provided.
fn override_sha() -> Option<String> {
    std::env::var("LATEST_COMMIT_SHA")
        .ok()
        .filter(|sha| !sha.is_empty() && sha != "dev")
        // A CI SHA is forty hex characters; the report carries a short one,
        // like `git rev-parse --short` produces locally.
        .map(|sha| sha.chars().take(12).collect())
}

/// Whether the tree the image was built from was dirty, when stated.
fn override_dirty() -> Option<bool> {
    std::env::var("AURCACHE_GIT_DIRTY")
        .ok()
        .filter(|dirty| !dirty.is_empty())
        .map(|dirty| dirty == "1")
}

/// Whether the checkout holds uncommitted changes.
///
/// Untracked files do not count, matching `git describe --dirty`: a scratch
/// file beside the tree must not mark every build from it as dirty.
fn git_dirty() -> bool {
    git(&["status", "--porcelain", "--untracked-files=no"]).is_some_and(|status| !status.is_empty())
}

fn main() {
    for var in [
        "LATEST_COMMIT_SHA",
        "AURCACHE_GIT_TAG",
        "AURCACHE_GIT_DIRTY",
    ] {
        println!("cargo:rerun-if-env-changed={var}");
    }
    println!("cargo:rerun-if-changed=build.rs");

    // Rebuild when the commit moves under us, so a commit does not keep
    // reporting its parent until an unrelated source edit rebuilds it.
    // Best-effort: exotic layouts (linked worktrees keep their refs
    // elsewhere) may miss a transition and report one commit stale.
    if let Some(dir) = git_dir() {
        // A checkout, a branch switch, and any commit on a detached HEAD all
        // rewrite HEAD itself.
        let head = dir.join("HEAD");
        watch(&head);
        if let Ok(contents) = std::fs::read_to_string(&head)
            && let Some(target) = contents.strip_prefix("ref: ").map(str::trim)
        {
            // A commit on a branch moves its ref instead: the loose ref,
            // when there is one (a `git gc` packs every ref away).
            watch(&dir.join(target));
        }
        // Every commit, checkout, and reset appends to the HEAD reflog,
        // whatever state the refs themselves are in.
        watch(&dir.join("logs").join("HEAD"));
        // Repacks move refs around without touching HEAD.
        watch(&dir.join("packed-refs"));
    }

    let sha = override_sha()
        .or_else(|| git(&["rev-parse", "--short=12", "HEAD"]))
        .unwrap_or_default();
    let tag = std::env::var("AURCACHE_GIT_TAG")
        .ok()
        .filter(|tag| !tag.is_empty())
        .or_else(|| git(&["describe", "--tags", "--exact-match", "HEAD"]))
        .unwrap_or_default();
    let dirty = override_dirty().unwrap_or_else(git_dirty);

    println!("cargo:rustc-env=AURCACHE_GIT_SHA={sha}");
    println!("cargo:rustc-env=AURCACHE_GIT_TAG={tag}");
    println!(
        "cargo:rustc-env=AURCACHE_GIT_DIRTY={}",
        if dirty { "1" } else { "" }
    );
}
