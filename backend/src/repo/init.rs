use anyhow::anyhow;
use flate2::read::GzEncoder;
use flate2::Compression;
use std::fs;
use std::fs::File;
use tokio::fs::symlink;

pub async fn init_repo_files() -> anyhow::Result<()> {
    // create repo folder
    if fs::metadata("./repo").is_err() {
        fs::create_dir("./repo")?;

        let tar_gz = File::create("./repo/repo.db.tar.gz")?;
        let enc = GzEncoder::new(tar_gz, Compression::default());
        let mut tar = tar::Builder::new(enc);
        tar.finish()
            .map_err(|_| anyhow!("failed to create repo archive"))?;
        symlink("repo.db.tar.gz", "./repo/repo.db")
            .await
            .map_err(|_| anyhow!("failed to create repo symlink"))?;

        let tar_gz = File::create("./repo/repo.files.tar.gz")?;
        let enc = GzEncoder::new(tar_gz, Compression::default());
        let mut tar = tar::Builder::new(enc);
        tar.finish()
            .map_err(|_| anyhow!("failed to create repo archive"))?;
        symlink("repo.files.tar.gz", "./repo/repo.files")
            .await
            .map_err(|_| anyhow!("failed to create repo symlink"))?;
    }
    Ok(())
}
