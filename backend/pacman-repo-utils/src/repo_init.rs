use anyhow::Context;
use flate2::Compression;
use flate2::write::GzEncoder;
use std::fs;
use std::fs::File;
use std::os::unix::fs::symlink;
use std::path::Path;
use tracing::info;

/// Create the repository `name` at `path`: its empty `db` and `files`
/// archives and the symlinks pacman reads them through.
///
/// Only what is missing is created. An archive that exists is never touched,
/// even when its symlink is gone: recreating it would empty a repository that
/// only lost a link.
pub fn init_repo(path: &Path, name: &str) -> anyhow::Result<()> {
    fs::create_dir_all(path)?;
    for suffix in ["db", "files"] {
        create_missing(path, name, suffix)?;
    }
    Ok(())
}

/// Link `<name>.db.sig` and `<name>.files.sig`, which pacman asks for, to the
/// archives' detached signatures, as `repo-add --sign` names them.
///
/// A link to a signature not written yet is left dangling, and answers "not
/// found" until there is one -- which is what a missing signature is.
pub fn link_signatures(path: &Path, name: &str) -> anyhow::Result<()> {
    for suffix in ["db", "files"] {
        let [signature, link] = get_archive_names(name, suffix).map(|n| format!("{n}.sig"));
        let link = path.join(link);
        if link.symlink_metadata().is_err() {
            symlink(signature, link).context("failed to link a database signature")?;
        }
    }
    Ok(())
}

/// Remove the databases' detached signatures and the links to them.
///
/// For a repository that has stopped signing: the next update rewrites the
/// databases unsigned, and a signature left beside them would then be a
/// wrong one, which pacman refuses even where signatures are optional.
pub fn remove_signatures(path: &Path, name: &str) -> anyhow::Result<()> {
    for suffix in ["db", "files"] {
        for file in get_archive_names(name, suffix) {
            match fs::remove_file(path.join(format!("{file}.sig"))) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e).context("failed to remove a database signature"),
            }
        }
    }
    Ok(())
}

/// assembles the filenames of the archive and its symlink, in that order
fn get_archive_names(name: &str, suffix: &str) -> [String; 2] {
    [
        format!("{name}.{suffix}.tar.gz"),
        format!("{name}.{suffix}"),
    ]
}

/// Create one archive, empty, and its symlink -- each only if it is missing.
fn create_missing(path: &Path, name: &str, suffix: &str) -> anyhow::Result<()> {
    let [archive_file_name, symlink_name] = get_archive_names(name, suffix);
    let archive_path = path.join(&archive_file_name);
    let symlink_path = path.join(&symlink_name);

    if !archive_path.exists() {
        info!(
            "Creating empty repository archive {}",
            archive_path.display()
        );
        let tar_gz = File::create(&archive_path)?;
        let enc = GzEncoder::new(tar_gz, Compression::default());
        let tar = tar::Builder::new(enc);
        // Finish explicitly: dropping the encoder would swallow a compression
        // error.
        tar.into_inner()
            .and_then(GzEncoder::finish)
            .context("failed to create repo archive")?;
    }
    // `symlink_metadata`, so a dangling link counts as present rather than
    // making `symlink` fail on an existing name.
    if symlink_path.symlink_metadata().is_err() {
        symlink(archive_file_name, symlink_path).context("failed to create repo symlink")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A repository that lost one symlink gets it back, and keeps its
    /// database: recreating the archives would have emptied it.
    #[test]
    fn a_missing_link_does_not_empty_the_repository() {
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path(), "repo").unwrap();
        fs::write(tmp.path().join("repo.db.tar.gz"), b"populated").unwrap();
        fs::remove_file(tmp.path().join("repo.files")).unwrap();

        init_repo(tmp.path(), "repo").unwrap();

        assert_eq!(
            fs::read(tmp.path().join("repo.db.tar.gz")).unwrap(),
            b"populated"
        );
        assert!(tmp.path().join("repo.files").exists());
    }

    /// pacman asks for `repo.db.sig`; it is a link to the archive's
    /// signature, made whether or not that is there yet, and removed with it.
    #[test]
    fn database_signatures_are_linked_where_pacman_asks_and_removed_together() {
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path(), "repo").unwrap();
        link_signatures(tmp.path(), "repo").unwrap();
        link_signatures(tmp.path(), "repo").unwrap();
        assert!(!tmp.path().join("repo.db.sig").exists(), "dangling for now");

        fs::write(tmp.path().join("repo.db.tar.gz.sig"), b"db").unwrap();
        fs::write(tmp.path().join("repo.files.tar.gz.sig"), b"files").unwrap();
        assert_eq!(fs::read(tmp.path().join("repo.db.sig")).unwrap(), b"db");
        assert_eq!(
            fs::read(tmp.path().join("repo.files.sig")).unwrap(),
            b"files"
        );

        remove_signatures(tmp.path(), "repo").unwrap();
        remove_signatures(tmp.path(), "repo").unwrap();
        for name in [
            "repo.db.sig",
            "repo.db.tar.gz.sig",
            "repo.files.sig",
            "repo.files.tar.gz.sig",
        ] {
            assert!(tmp.path().join(name).symlink_metadata().is_err(), "{name}");
        }
        assert!(tmp.path().join("repo.db.tar.gz").exists());
    }
}
