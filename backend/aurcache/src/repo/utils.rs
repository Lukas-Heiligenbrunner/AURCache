use crate::db::prelude::PackagesFiles;
use crate::db::{files, packages_files};
use log::{info, warn};
use sea_orm::ColumnTrait;
use sea_orm::QueryFilter;
use sea_orm::{DatabaseTransaction, EntityTrait, ModelTrait};
use std::fs;

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

        pacman_repo_utils::repo_remove(
            file.filename.clone(),
            format!("./repo/{platform}/repo.db.tar.gz"),
            format!("./repo/{platform}/repo.files.tar.gz"),
        )?;

        let file_path = format!("./repo/{}/{}", platform, file.filename);
        match fs::remove_file(file_path.clone()) {
            Ok(_) => info!("Removed old file: {}", file_path),
            Err(_) => warn!("Failed to remove package file: {}", file_path),
        }

        file.delete(db).await?;
    }

    Ok(())
}
