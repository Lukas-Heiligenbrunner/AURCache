use crate::aur::aur::get_info_by_name;
use crate::builder::types::Action;
use crate::db::prelude::Packages;
use crate::db::{builds, packages, versions};
use anyhow::anyhow;
use sea_orm::ColumnTrait;
use sea_orm::QueryFilter;
use sea_orm::{ActiveModelTrait, DatabaseConnection, EntityTrait, Set, TransactionTrait};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::broadcast::Sender;

pub async fn package_add(
    db: &DatabaseConnection,
    pkg_name: String,
    tx: &Sender<Action>,
) -> anyhow::Result<()> {
    let txn = db.begin().await?;

    // remove leading and trailing whitespaces
    let pkg_name = pkg_name.trim();

    if Packages::find()
        .filter(packages::Column::Name.eq(pkg_name))
        .one(&txn)
        .await?
        .is_some()
    {
        return Err(anyhow!("Package already exists"));
    }

    let pkg = get_info_by_name(pkg_name).await?;

    let new_package = packages::ActiveModel {
        name: Set(pkg_name.to_string()),
        status: Set(3),
        latest_aur_version: Set(pkg.version.clone()),
        ..Default::default()
    };

    let mut new_package = new_package.clone().save(&txn).await?;

    let new_version = versions::ActiveModel {
        version: Set(pkg.version.clone()),
        package_id: new_package.id.clone(),
        ..Default::default()
    };

    let new_version = new_version.clone().save(&txn).await?;

    new_package.status = Set(3);
    new_package.latest_version_id = Set(Some(new_version.id.clone().unwrap()));
    new_package.save(&txn).await?;

    // set build status to pending
    let build = builds::ActiveModel {
        pkg_id: new_version.package_id.clone(),
        version_id: new_version.id.clone(),
        ouput: Set(None),
        status: Set(Some(3)),
        start_time: Set(Some(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs() as u32,
        )),
        ..Default::default()
    };
    let new_build = build.save(&txn).await?;

    let _ = tx.send(Action::Build(
        pkg.name,
        pkg.version,
        pkg.url_path.unwrap(),
        new_version,
        new_build,
    ));

    txn.commit().await?;

    Ok(())
}
