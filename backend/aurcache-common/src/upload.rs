//! The files an upload of packages built elsewhere may carry, by name: what
//! the server accepts, and what a client can check before sending any.

/// The most files one upload may carry, signatures included.
pub const MAX_FILES: usize = 64;

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

/// A file an upload may carry, by its name ([`FileKind::of`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct File {
    name: String,
    kind: FileKind,
}

impl File {
    /// The file `filename` names, when it is one an upload may carry.
    ///
    /// # Errors
    /// Why the name is not one an upload may carry ([`FileKind::of`]).
    pub fn new(filename: &str) -> Result<Self, String> {
        Ok(Self {
            kind: FileKind::of(filename)?,
            name: filename.to_string(),
        })
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub fn kind(&self) -> FileKind {
        self.kind
    }
}

/// The files of an upload, taken one at a time as the server takes them
/// while they arrive: none twice, and at most [`MAX_FILES`].
#[derive(Debug, Default)]
pub struct Names {
    /// The files taken so far, in the order they came.
    taken: Vec<File>,
}

impl Names {
    /// Take `file` as the upload's next file.
    ///
    /// # Errors
    /// Why the upload cannot carry it as well as the files before it.
    pub fn take(&mut self, file: File) -> Result<(), String> {
        if self.taken.iter().any(|taken| taken.name == file.name) {
            return Err(format!("'{}' is in the upload twice", file.name));
        }
        if self.taken.len() >= MAX_FILES {
            return Err(format!("an upload carries at most {MAX_FILES} files"));
        }
        self.taken.push(file);
        Ok(())
    }

    /// The files taken so far, in the order they came.
    #[must_use]
    pub fn taken(&self) -> &[File] {
        &self.taken
    }
}

/// Check the names of an upload's files, as the server will as they arrive
/// ([`File::new`], [`Names`]).
///
/// # Errors
/// What is wrong with the first name that fails.
pub fn check_names<'a>(filenames: impl IntoIterator<Item = &'a str>) -> Result<(), String> {
    let mut names = Names::default();
    for filename in filenames {
        names.take(File::new(filename)?)?;
    }
    Ok(())
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

    #[test]
    fn an_upload_names_each_file_once_and_not_too_many() {
        let package = "hello-1.0-1-x86_64.pkg.tar.zst";
        let signature = "hello-1.0-1-x86_64.pkg.tar.zst.sig";
        assert_eq!(check_names([package, signature]), Ok(()));
        assert!(
            check_names([package, package])
                .unwrap_err()
                .contains("twice")
        );
        assert!(check_names(["PKGBUILD"]).unwrap_err().contains("PKGBUILD"));

        let names: Vec<String> = (0..=MAX_FILES)
            .map(|n| format!("p{n}-1.0-1-x86_64.pkg.tar.zst"))
            .collect();
        assert_eq!(
            check_names(names[..MAX_FILES].iter().map(String::as_str)),
            Ok(())
        );
        assert!(
            check_names(names.iter().map(String::as_str))
                .unwrap_err()
                .contains("at most")
        );
    }
}
