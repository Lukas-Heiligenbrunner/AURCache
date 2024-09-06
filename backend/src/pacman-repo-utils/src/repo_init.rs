use anyhow::anyhow;
use flate2::read::GzEncoder;
use flate2::Compression;
use log::info;
use std::fs;
use std::fs::File;
use std::os::unix::fs::symlink;
use std::path::PathBuf;

pub fn init_repo_impl(path: &PathBuf, name: &str) -> anyhow::Result<()> {
    let db_file = path.join(format!("{}.db.tar.gz", name));
    let files_file = path.join(format!("{}.files.tar.gz", name));

    // create repo folder
    if fs::metadata(&db_file).is_ok() || fs::metadata(&files_file).is_ok() {
        info!("Pacman repo archive already exists");
        return Ok(());
    }

    info!("Initializing empty pacman Repo archive");
    _ = fs::create_dir(path);

    let tar_gz = File::create(&db_file)?;
    let enc = GzEncoder::new(tar_gz, Compression::default());
    let mut tar = tar::Builder::new(enc);
    tar.finish()
        .map_err(|_| anyhow!("failed to create repo archive"))?;
    symlink(&db_file, path.join(format!("{}.db", name)))
        .map_err(|_| anyhow!("failed to create repo symlink"))?;

    let tar_gz = File::create(&files_file)?;
    let enc = GzEncoder::new(tar_gz, Compression::default());
    let mut tar = tar::Builder::new(enc);
    tar.finish()
        .map_err(|_| anyhow!("failed to create repo archive"))?;
    symlink(&files_file, path.join(format!("{}.files", name)))
        .map_err(|_| anyhow!("failed to create repo symlink"))?;
    Ok(())
}
