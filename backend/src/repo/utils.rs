use crate::db::prelude::PackagesFiles;
use crate::db::{files, packages_files};
use anyhow::anyhow;
use log::info;
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
        let filename = file.filename.clone();
        file.delete(db).await?;

        pacman_repo_utils::repo_remove(
            filename.clone(),
            "./repo/repo.db.tar.gz".to_string(),
            "./repo/repo.files.tar.gz".to_string(),
        )?;
        fs::remove_file(format!("./repo/{}", filename))?;

        info!("Removed old file: {}", filename);
    }

    Ok(())
}
