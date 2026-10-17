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
}
