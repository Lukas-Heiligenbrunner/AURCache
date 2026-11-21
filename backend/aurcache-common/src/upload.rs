//! The files an upload of packages built elsewhere may carry, by name: what
//! the server accepts, and what a client can check before sending any.

/// What a file in an upload is, from its name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileKind {
    /// A package archive, `*.pkg.tar.zst` or `*.pkg.tar.xz`.
    Package,
    /// A detached signature of one, the archive's name plus `.sig`.
    Signature,
}

impl FileKind {
    /// The kind of file `filename` names, when it is one an upload may carry.
    ///
    /// Only a bare name is accepted -- no directory, nothing dot-prefixed -- so
    /// a file can only ever land in the directory it is written to.
    ///
    /// # Errors
    /// Why the name is not one an upload may carry.
    pub fn of(filename: &str) -> Result<Self, String> {
        let bare = !filename.is_empty()
            && filename.len() <= 255
            && !filename.starts_with('.')
            && !filename.contains(['/', '\\', '\0']);
        if !bare {
            return Err(format!("'{filename}' is not a bare file name"));
        }
        let package = |name: &str| name.ends_with(".pkg.tar.zst") || name.ends_with(".pkg.tar.xz");
        if package(filename) {
            Ok(Self::Package)
        } else if filename.strip_suffix(".sig").is_some_and(package) {
            Ok(Self::Signature)
        } else {
            Err(format!(
                "'{filename}' is neither a package (*.pkg.tar.zst, *.pkg.tar.xz) nor its .sig"
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_bare_package_and_signature_names_are_accepted() {
        assert_eq!(
            FileKind::of("hello-1.0-1-x86_64.pkg.tar.zst"),
            Ok(FileKind::Package)
        );
        assert_eq!(
            FileKind::of("hello-1.0-1-x86_64.pkg.tar.xz.sig"),
            Ok(FileKind::Signature)
        );
        for name in [
            "",
            ".hidden.pkg.tar.zst",
            "../hello-1.0-1-x86_64.pkg.tar.zst",
            "dir/hello-1.0-1-x86_64.pkg.tar.zst",
            "dir\\hello-1.0-1-x86_64.pkg.tar.zst",
            "hello-1.0-1-x86_64.pkg.tar.gz",
            "hello.sig",
            "PKGBUILD",
        ] {
            assert!(FileKind::of(name).is_err(), "{name:?} is refused");
        }
    }
}
