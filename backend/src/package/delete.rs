use crate::db::builds;
use crate::db::prelude::{Builds, Packages};
use anyhow::anyhow;
use sea_orm::ColumnTrait;
use sea_orm::QueryFilter;
use sea_orm::{DatabaseConnection, EntityTrait, ModelTrait, TransactionTrait};
use std::fs;

pub async fn package_delete(db: &DatabaseConnection, pkg_id: i32) -> anyhow::Result<()> {
    let txn = db.begin().await?;

    let pkg = Packages::find_by_id(pkg_id)
        .one(&txn)
        .await?
        .ok_or(anyhow!("id not found"))?;

    // remove build dir if available
    let _ = fs::remove_dir_all(format!("./builds/{}", pkg.name));

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

    txn.commit().await?;

    Ok(())
}
