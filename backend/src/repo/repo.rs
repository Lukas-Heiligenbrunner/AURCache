use crate::db::prelude::Packages;
use crate::db::prelude::Versions;
use crate::db::versions;
use anyhow::anyhow;
use sea_orm::{
    ColumnTrait, DatabaseConnection, DatabaseTransaction, EntityTrait, ModelTrait, QueryFilter,
    TransactionTrait,
};
use std::fs;
use std::process::Command;

static REPO_NAME: &str = "repo";

pub async fn remove_version(db: &DatabaseConnection, version_id: i32) -> anyhow::Result<()> {
    let txn = db.begin().await?;

    let version = Versions::find()
        .filter(versions::Column::PackageId.eq(version_id))
        .one(&txn)
        .await?;
    if let Some(version) = version {
        rem_ver(&txn, version).await?;
    }

    txn.commit().await?;

    Ok(())
}

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

    println!("{db_file} updated successfully");
    Ok(())
}

fn repo_remove(pkg_file_name: String) -> anyhow::Result<()> {
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

    println!("{db_file} updated successfully");
    Ok(())
}

pub(crate) async fn rem_ver(
    db: &DatabaseTransaction,
    version: versions::Model,
) -> anyhow::Result<()> {
    if let Some(filename) = version.file_name.clone() {
        // so repo-remove only supports passing a package name and removing the whole package
        // it seems that repo-add removes an older version when called
        // todo fix in future by implementing in rust
        if let Some(pkg) = Packages::find_by_id(version.package_id).one(db).await? {
            // remove from repo db
            repo_remove(pkg.name)?;

            // remove from fs
            fs::remove_file(format!("./repo/{filename}"))?;
        }
    }

    version.delete(db).await?;
    Ok(())
}
