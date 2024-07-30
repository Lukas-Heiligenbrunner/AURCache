use crate::db::prelude::PackagesFiles;
use crate::db::{files, packages_files};
use anyhow::anyhow;
use log::info;
use sea_orm::ColumnTrait;
use sea_orm::QueryFilter;
use sea_orm::{DatabaseTransaction, EntityTrait, ModelTrait};
use std::fs;
use std::process::Command;

static REPO_NAME: &str = "repo";

pub fn repo_add(pkg_file_name: String) -> anyhow::Result<()> {
    let db_file = format!("{REPO_NAME}.db.tar.gz");

    let output = Command::new("repo-add")
        .args(&[db_file.clone(), pkg_file_name, "--nocolor".to_string()])
        .current_dir("./repo/")
        .output()?;

    if !output.status.success() {
        return Err(anyhow!(
            "Error exit code when repo-add: {}{}",
            String::from_utf8_lossy(output.stdout.as_slice()),
            String::from_utf8_lossy(output.stderr.as_slice())
        ));
    }

    info!("{db_file} updated successfully");
    Ok(())
}

pub fn repo_remove(pkg_file_name: String) -> anyhow::Result<()> {
    let db_file = format!("{REPO_NAME}.db.tar.gz");

    let output = Command::new("repo-remove")
        .args(&[db_file.clone(), pkg_file_name, "--nocolor".to_string()])
        .current_dir("./repo/")
        .output()?;

    if !output.status.success() {
        return Err(anyhow!(
            "Error exit code when repo-remove: {}{}",
            String::from_utf8_lossy(output.stdout.as_slice()),
            String::from_utf8_lossy(output.stderr.as_slice())
        ));
    }

    info!("{db_file} updated successfully");
    Ok(())
}

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
        let _ = repo_remove(filename.clone());
        fs::remove_file(format!("./repo/{}", filename))?;

        info!("Removed old file: {}", filename);
    }

    Ok(())
}
