use aurcache_db::files;
use sea_orm::{DatabaseTransaction, ModelTrait};
use std::fs;
use tracing::{info, warn};

pub async fn try_remove_archive_file(
    file: files::Model,
    db: &DatabaseTransaction,
) -> anyhow::Result<()> {
    let platform = file.platform.clone();

    pacman_repo_utils::repo_remove::repo_remove(
        file.filename.clone(),
        format!("./repo/{platform}/repo.db.tar.gz"),
        format!("./repo/{platform}/repo.files.tar.gz"),
    )?;

    let file_path = format!("./repo/{}/{}", platform, file.filename);
    if let Ok(()) = fs::remove_file(file_path.clone()) {
        info!("Removed old file: {file_path}")
    } else {
        warn!("Failed to remove package file: {file_path}")
    }

    file.delete(db).await?;

    Ok(())
}
