use crate::aur::api::get_info_by_name;
use crate::builder::types::Action;
use crate::db::prelude::Packages;
use crate::db::{builds, packages};
use anyhow::anyhow;
use sea_orm::{ActiveModelTrait, DatabaseConnection, EntityTrait, Set, TransactionTrait};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::broadcast::Sender;

pub async fn package_update(
    db: &DatabaseConnection,
    pkg_id: i32,
    force: bool,
    tx: &Sender<Action>,
) -> anyhow::Result<i32> {
    let txn = db.begin().await?;

    let mut pkg_model_active: packages::ActiveModel = Packages::find_by_id(pkg_id)
        .one(&txn)
        .await?
        .ok_or(anyhow!("id not found"))?
        .into();
    let pkg_model: packages::Model = pkg_model_active.clone().try_into()?;

    let pkg = get_info_by_name(pkg_model.name.as_str())
        .await
        .map_err(|_| anyhow!("couldn't download package metadata".to_string()))?;

    if !force && pkg_model.version == Some(pkg.version.clone()) {
        return Err(anyhow!("Package is already up to date"));
    }

    pkg_model_active.status = Set(3);
    pkg_model_active.version = Set(Some(pkg.version.clone()));
    let pkg_model = pkg_model_active.save(&txn).await?;

    // set build status to pending
    let start_time = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
    let build = builds::ActiveModel {
        pkg_id: pkg_model.id.clone(),
        output: Set(None),
        status: Set(Some(3)),
        start_time: Set(Some(start_time)),
        ..Default::default()
    };
    let new_build = build.save(&txn).await?;
    let build_id = new_build.id.clone().unwrap();
    txn.commit().await?;

    let _ = tx.send(Action::Build(Box::from(pkg_model), Box::from(new_build)));

    Ok(build_id)
}
