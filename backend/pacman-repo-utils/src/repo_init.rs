use anyhow::{anyhow, bail};
use flate2::Compression;
use flate2::read::GzEncoder;
use log::info;
use std::fs;
use std::fs::File;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

pub fn init_repo_impl(path: &PathBuf, name: &str) -> anyhow::Result<()> {
    if repo_exists(path, name).is_ok() {
        info!(
            "Pacman repo '{}' archive already exists at path '{}'",
            name,
            path.display()
        );
        return Ok(());
    }

    // create repo folder
    info!("Initializing empty pacman Repo archive");
    _ = fs::create_dir_all(path);

    create_empty_archive(path, name, "db")?;
    create_empty_archive(path, name, "files")?;
    Ok(())
}

/// check if repo archives and symlink exist
fn repo_exists(path: &Path, name: &str) -> anyhow::Result<()> {
    for suffix in ["db", "files"] {
        let files = get_archive_names(name, suffix);
        for file in [files.0, files.1] {
            if fs::metadata(path.join(&file)).is_err() {
                bail!("{} doesn't exist", file);
            }
        }
    }
    Ok(())
}

/// assembles filneame of archive and symlink
fn get_archive_names(name: &str, suffix: &str) -> (String, String) {
    let file_name = format!("{}.{}.tar.gz", name, suffix);
    let symlink_name = format!("{}.{}", name, suffix);
    (file_name, symlink_name)
}

/// create empty archive and corresponding symlink
fn create_empty_archive(path: &Path, name: &str, suffix: &str) -> anyhow::Result<()> {
    let (archive_file_name, symlink_name) = get_archive_names(name, suffix);
    let archive_path = path.join(&archive_file_name);
    let symlink_path = path.join(&symlink_name);

    let tar_gz = File::create(archive_path)?;
    let enc = GzEncoder::new(tar_gz, Compression::default());
    let mut tar = tar::Builder::new(enc);
    tar.finish()
        .map_err(|_| anyhow!("failed to create repo archive"))?;
    symlink(archive_file_name, symlink_path)
        .map_err(|_| anyhow!("failed to create repo symlink"))?;
    Ok(())
}
