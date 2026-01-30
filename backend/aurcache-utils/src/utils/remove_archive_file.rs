use aurcache_db::prelude::PackagesFiles;
use aurcache_db::{files, packages_files};
use sea_orm::ColumnTrait;
use sea_orm::QueryFilter;
use sea_orm::{DatabaseTransaction, EntityTrait, ModelTrait};
use std::fs;
use tracing::{info, warn};

pub async fn try_remove_archive_file(
    file: files::Model,
    db: &DatabaseTransaction,
) -> anyhow::Result<()> {
    let package_files = PackagesFiles::find()
        .filter(packages_files::Column::FileId.eq(file.id))
        .all(db)
        .await?;
    if package_files.is_empty() {
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
    }

    Ok(())
}
