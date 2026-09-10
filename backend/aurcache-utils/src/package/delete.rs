use crate::utils::remove_archive_file::try_remove_archive_file;
use anyhow::anyhow;
use aurcache_db::prelude::{Builds, Files, Packages, Settings};
use aurcache_db::{builds, files, settings};
use sea_orm::{
    ColumnTrait, DatabaseConnection, EntityTrait, ModelTrait, QueryFilter, TransactionTrait,
};

pub async fn package_delete(db: &DatabaseConnection, pkg_id: i32) -> anyhow::Result<()> {
    let txn = db.begin().await?;

    let pkg = Packages::find_by_id(pkg_id)
        .one(&txn)
        .await?
        .ok_or(anyhow!("id not found"))?;

    // remove package db entry
    pkg.clone().delete(&txn).await?;

    // remove corresponding builds
    let builds = Builds::find()
        .filter(builds::Column::PkgId.eq(pkg.id))
        .all(&txn)
        .await?;
    for b in builds {
        b.delete(&txn).await?;
    }

    // remove package files
    let package_files: Vec<files::Model> = Files::find()
        .filter(files::Column::PackageId.eq(pkg.id))
        .all(&txn)
        .await?;

    for file in package_files {
        try_remove_archive_file(file, &txn).await?;
    }

    // delete corresponding settings entries
    Settings::delete_many()
        .filter(settings::Column::PkgId.eq(pkg.id))
        .exec(&txn)
        .await?;

    txn.commit().await?;

    Ok(())
}
