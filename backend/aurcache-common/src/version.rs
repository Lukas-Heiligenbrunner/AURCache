//! The version string binaries report.
//!
//! Every binary reports its own crate release, plus the commit when the build
//! is not exactly that release: `0.5.0` for a release, `0.5.0+g8afa04a` for a
//! development build, `.dirty` appended when the tree had uncommitted changes.
//! The commit is what tells two builds of one release apart — which, for a
//! build server, is the point of reporting a version at all.
//!
//! No parsing is done on these strings anywhere: a release and a development
//! build of it are different answers to "what is running", not values to
//! compare, so workers and servers only ever display them.

/// The version for a binary to report: its crate release, plus the commit
/// when the build is not exactly that release.
///
/// `crate_version` is the caller's own `env!("CARGO_PKG_VERSION")` — each
/// binary reports its own release line, so this helper takes it as a
/// parameter rather than baking in this crate's.
pub fn full_version(crate_version: &str) -> String {
    assemble_version(
        crate_version,
        option_env!("AURCACHE_GIT_TAG").unwrap_or(""),
        option_env!("AURCACHE_GIT_SHA").unwrap_or(""),
        option_env!("AURCACHE_GIT_DIRTY").unwrap_or("") == "1",
    )
}

/// Assemble a reported version from its parts.
///
/// A release tag (`v*`, exactly at this commit) or no git information at all
/// (a crates.io or tag-tarball build) reports the bare release: such
/// artifacts are releases by construction, and the release name stands on its
/// own. Anything else names the commit as build metadata, so a development or
/// snapshot build can never be mistaken for the release it will become.
pub fn assemble_version(crate_version: &str, tag: &str, sha: &str, dirty: bool) -> String {
    if tag.strip_prefix('v').is_some_and(|rest| !rest.is_empty()) {
        return crate_version.to_string();
    }
    if sha.is_empty() {
        return crate_version.to_string();
    }
    let mut version = format!("{crate_version}+g{sha}");
    if dirty {
        version.push_str(".dirty");
    }
    version
}

#[cfg(test)]
mod tests {
    use super::{assemble_version, full_version};

    /// A tagged build reports the bare release, even with a commit and a
    /// dirty tree alongside: the tag is what marks it as a release.
    #[test]
    fn a_release_tag_reports_the_bare_release() {
        assert_eq!(
            assemble_version("0.5.0", "v0.5.0", "8afa04a", false),
            "0.5.0"
        );
        assert_eq!(
            assemble_version("0.1.0", "v0.5.0", "8afa04a", true),
            "0.1.0"
        );
    }

    /// A development build names its commit, so two builds of one release
    /// tell apart.
    #[test]
    fn past_a_release_the_commit_is_named() {
        assert_eq!(
            assemble_version("0.5.0", "", "8afa04a", false),
            "0.5.0+g8afa04a"
        );
    }

    /// An uncommitted tree says so: a dirty build is never mistaken for the
    /// clean one at the same commit.
    #[test]
    fn a_dirty_tree_says_so() {
        assert_eq!(
            assemble_version("0.5.0", "", "8afa04a", true),
            "0.5.0+g8afa04a.dirty"
        );
    }

    /// No git information (a crates.io or tag-tarball build) reports the
    /// bare release: there is no commit to name.
    #[test]
    fn without_git_there_is_only_the_release() {
        assert_eq!(assemble_version("0.5.0", "", "", false), "0.5.0");
    }

    /// A non-release tag is not a release marker: the commit is still named.
    #[test]
    fn only_release_tags_mark_a_release() {
        assert_eq!(
            assemble_version("0.5.0", "nightly", "8afa04a", false),
            "0.5.0+g8afa04a"
        );
        assert_eq!(
            assemble_version("0.5.0", "v", "8afa04a", false),
            "0.5.0+g8afa04a"
        );
    }

    /// The plumbing holds whatever the checkout says: the reported version
    /// is always this crate's release, with at most a commit appended.
    #[test]
    fn the_reported_version_is_this_crates_release() {
        let release = env!("CARGO_PKG_VERSION");
        let version = full_version(release);
        assert!(
            version == release || version.starts_with(&format!("{release}+g")),
            "{version}"
        );
    }
}
