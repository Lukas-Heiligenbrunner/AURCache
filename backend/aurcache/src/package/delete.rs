use crate::db::migration::JoinType;
use crate::db::prelude::{Builds, Packages, PackagesFiles};
use crate::db::{builds, files, packages_files};
use crate::repo::utils::try_remove_archive_file;
use anyhow::anyhow;
use sea_orm::QueryFilter;
use sea_orm::{ColumnTrait, QuerySelect, RelationTrait};
use sea_orm::{DatabaseConnection, EntityTrait, ModelTrait, TransactionTrait};

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

        try_remove_archive_file(file.unwrap(), &txn).await?;
    }

    txn.commit().await?;

    Ok(())
}
