//! Which file names are built packages.
//!
//! A worker asks it of every name a build left before uploading the file,
//! and the name then becomes a URL path segment and a file in the
//! repository. It is here, beside the types every party speaks, so that
//! anything else that handles a build's output asks the same question the
//! same way: two copies of the rule would let one accept what the other
//! refuses.

/// True for a built package artifact filename.
///
/// The character allow-list matters: these names come from whatever the
/// PKGBUILD wrote into the build directory and are then used as a URL path
/// segment and a repo-tree filename. Restricting to the set alpm actually
/// produces (`pkgname-pkgver-pkgrel-arch.pkg.tar.*`, where epoch contributes
/// `:` and pkgver may contain `+`, `.`, `_`, `~`) keeps a hostile or merely
/// broken PKGBUILD from smuggling separators or traversal through either.
#[must_use]
pub fn is_artifact(name: &str) -> bool {
    !name.starts_with('.')
        && name.contains(".pkg.tar")
        && !name.ends_with(".sig")
        && !name.ends_with(".part")
        && name.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '+' | ':' | '~' | '@')
        })
}

#[cfg(test)]
mod tests {
    use super::is_artifact;

    #[test]
    fn is_artifact_rules() {
        assert!(is_artifact("a-1-1-x86_64.pkg.tar.zst"));
        assert!(is_artifact("a-1-1-x86_64.pkg.tar.xz"));
        assert!(!is_artifact("a-1-1-x86_64.pkg.tar.zst.sig"));
        assert!(!is_artifact(".x.pkg.tar.zst"));
        assert!(!is_artifact("PKGBUILD"));
    }

    /// Real alpm version syntax must survive the character allow-list.
    #[test]
    fn is_artifact_accepts_epoch_and_pkgver_punctuation() {
        assert!(is_artifact("foo-2:1.0_beta+3~rc1-1-x86_64.pkg.tar.zst"));
        assert!(is_artifact("lib32-gcc-libs-14.2-1-x86_64.pkg.tar.zst"));
    }

    /// Names that would corrupt the upload URL or escape the repo directory.
    #[test]
    fn is_artifact_rejects_url_and_path_hostile_names() {
        assert!(!is_artifact("evil?x=1.pkg.tar.zst"));
        assert!(!is_artifact("evil#frag.pkg.tar.zst"));
        assert!(!is_artifact("../../etc/passwd.pkg.tar.zst"));
        assert!(!is_artifact("a b-1-1-x86_64.pkg.tar.zst"));
        assert!(!is_artifact("a%2f-1-1-x86_64.pkg.tar.zst"));
    }
}
