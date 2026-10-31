//! Multi-file source patches.
//!
//! A [`SourcePatch`] describes a set of per-file unified diffs that get
//! applied to a package's fetched source (AUR snapshot, git checkout, or
//! future uploaded archive) before it is handed to the builder. This lets
//! users tweak a `PKGBUILD` (or any other tracked source file) without
//! forking the upstream package.
//!
//! Patches only ever target `PKGBUILD`/plain source files - `.SRCINFO` is
//! never part of a stored patch. Whenever a patch is present, `.SRCINFO` is
//! always regenerated from the (possibly patched) `PKGBUILD` after the patch
//! has been applied, so editing `PKGBUILD` alone is enough to update
//! metadata such as dependencies or the package version.

use std::collections::BTreeMap;

use diffy::Patch;
use serde::{Deserialize, Serialize};

/// A collection of per-file unified diffs, keyed by the file's path relative
/// to the source root (e.g. `PKGBUILD`, `foo.install`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SourcePatch {
    files: BTreeMap<String, String>,
}

impl SourcePatch {
    /// Whether this patch doesn't change anything.
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Parse a patch previously persisted with [`SourcePatch::to_json`].
    ///
    /// Returns an empty patch for blank input so callers can treat "no
    /// patch stored" and "empty patch stored" identically.
    pub fn parse(raw: &str) -> anyhow::Result<Self> {
        if raw.trim().is_empty() {
            return Ok(Self::default());
        }
        Ok(serde_json::from_str(raw)?)
    }

    /// Serialize this patch for storage in `packages.patch`.
    pub fn to_json(&self) -> anyhow::Result<String> {
        Ok(serde_json::to_string(&self)?)
    }

    /// What `packages.patch` holds for this patch: nothing for one that
    /// changes nothing, so an emptied patch reads as no patch at all.
    pub fn stored(&self) -> anyhow::Result<Option<String>> {
        if self.is_empty() {
            Ok(None)
        } else {
            self.to_json().map(Some)
        }
    }

    /// List the files touched by this patch.
    pub fn paths(&self) -> impl Iterator<Item = &str> {
        self.files.keys().map(String::as_str)
    }

    /// Look up the stored unified diff for a single file, if any.
    pub fn diff_for(&self, rel_path: &str) -> Option<&str> {
        self.files.get(rel_path).map(String::as_str)
    }

    /// Apply the effective (patched) content for a single file onto `content`,
    /// returning `content` unchanged if the file isn't part of this patch.
    pub fn apply_to_content(&self, rel_path: &str, content: &str) -> anyhow::Result<String> {
        match self.diff_for(rel_path) {
            None => Ok(content.to_string()),
            Some(diff_text) => {
                let patch = Patch::from_str(diff_text)
                    .map_err(|e| anyhow::anyhow!("Invalid patch for '{rel_path}': {e}"))?;
                diffy::apply(content, &patch)
                    .map_err(|e| anyhow::anyhow!("Failed to apply patch to '{rel_path}': {e}"))
            }
        }
    }

    /// Record a user's edit of a single file: `original_content` is the
    /// pristine (unpatched) content of the file and `new_content` is the
    /// content the user saved. If they're identical, any existing diff for
    /// that file is dropped.
    pub fn merge_file(&mut self, rel_path: &str, original_content: &str, new_content: &str) {
        if original_content == new_content {
            self.files.remove(rel_path);
            return;
        }
        let diff = diffy::create_patch(original_content, new_content);
        self.files.insert(rel_path.to_string(), diff.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_patch_roundtrips() {
        let patch = SourcePatch::default();
        assert!(patch.is_empty());
        let json = patch.to_json().unwrap();
        assert_eq!(SourcePatch::parse(&json).unwrap(), patch);
        assert_eq!(SourcePatch::parse("").unwrap(), SourcePatch::default());
        assert_eq!(SourcePatch::parse("   ").unwrap(), SourcePatch::default());
    }

    #[test]
    fn merge_file_adds_and_removes_diff() {
        let mut patch = SourcePatch::default();
        patch.merge_file("PKGBUILD", "pkgver=1\n", "pkgver=2\n");
        assert!(!patch.is_empty());
        assert_eq!(patch.paths().collect::<Vec<_>>(), vec!["PKGBUILD"]);

        // Reverting the edit back to the original content removes the diff again.
        patch.merge_file("PKGBUILD", "pkgver=1\n", "pkgver=1\n");
        assert!(patch.is_empty());
    }

    #[test]
    fn apply_to_content_reflects_merged_edit() {
        let mut patch = SourcePatch::default();
        let original = "pkgname=foo\npkgver=1\npkgrel=1\n";
        let edited = "pkgname=foo\npkgver=2\npkgrel=1\n";
        patch.merge_file("PKGBUILD", original, edited);

        let applied = patch.apply_to_content("PKGBUILD", original).unwrap();
        assert_eq!(applied, edited);

        // Files untouched by the patch pass through unchanged.
        let other = patch.apply_to_content("other.install", "hello\n").unwrap();
        assert_eq!(other, "hello\n");
    }
}
