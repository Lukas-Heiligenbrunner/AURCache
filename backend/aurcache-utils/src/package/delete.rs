use crate::utils::remove_archive_file::try_remove_archive_file;
use anyhow::anyhow;
use aurcache_db::prelude::{Builds, Packages, PackagesFiles, Settings};
use aurcache_db::{builds, files, packages_files, settings};
use sea_orm::{ColumnTrait, QuerySelect, RelationTrait};
use sea_orm::{DatabaseConnection, EntityTrait, ModelTrait, TransactionTrait};
use sea_orm::{JoinType, QueryFilter};

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
    let package_files: Vec<(packages_files::Model, Option<files::Model>)> = PackagesFiles::find()
        .filter(packages_files::Column::PackageId.eq(pkg.id))
        .join(JoinType::LeftJoin, packages_files::Relation::Files.def())
        .select_also(files::Entity)
        .all(&txn)
        .await?;

    for (pf, file) in package_files {
        pf.delete(&txn).await?;

        try_remove_archive_file(
            file.ok_or(anyhow!("package id has no attached file"))?,
            &txn,
        )
        .await?;
    }

    // delete corresponding settings entries
    Settings::delete_many()
        .filter(settings::Column::PkgId.eq(pkg.id))
        .exec(&txn)
        .await?;

    txn.commit().await?;

    Ok(())
}
