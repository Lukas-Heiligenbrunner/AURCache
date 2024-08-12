use std::fs;
use std::fs::File;
use std::os::unix::fs::symlink;
use anyhow::anyhow;
use flate2::Compression;
use flate2::read::GzEncoder;
use log::info;

pub fn init_repo_impl() -> anyhow::Result<()>{
    // create repo folder
    if fs::metadata("./repo").is_err() {
        info!("Initializing empty pacman Repo archive");
        fs::create_dir("./repo")?;

        let tar_gz = File::create("./repo/repo.db.tar.gz")?;
        let enc = GzEncoder::new(tar_gz, Compression::default());
        let mut tar = tar::Builder::new(enc);
        tar.finish()
            .map_err(|_| anyhow!("failed to create repo archive"))?;
        symlink("repo.db.tar.gz", "./repo/repo.db")
            .map_err(|_| anyhow!("failed to create repo symlink"))?;

        let tar_gz = File::create("./repo/repo.files.tar.gz")?;
        let enc = GzEncoder::new(tar_gz, Compression::default());
        let mut tar = tar::Builder::new(enc);
        tar.finish()
            .map_err(|_| anyhow!("failed to create repo archive"))?;
        symlink("repo.files.tar.gz", "./repo/repo.files")
            .map_err(|_| anyhow!("failed to create repo symlink"))?;
    }
    Ok(())
}