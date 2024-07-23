use crate::aur::aur::get_info_by_name;
use crate::builder::types::Action;
use crate::db::prelude::{Packages};
use crate::db::{builds, packages};
use anyhow::anyhow;
use sea_orm::ColumnTrait;
use sea_orm::QueryFilter;
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

    let mut pkg_model: packages::ActiveModel = Packages::find_by_id(pkg_id)
        .one(&txn)
        .await?
        .ok_or(anyhow!("id not found"))?
        .into();

    let pkg = get_info_by_name(pkg_model.name.clone().unwrap().as_str())
        .await
        .map_err(|_| anyhow!("couldn't download package metadata".to_string()))?;

    pkg_model.status = Set(3);
    let pkg_model = pkg_model.save(&txn).await?;

    // set build status to pending
    let build = builds::ActiveModel {
        pkg_id: pkg_model.id.clone(),
        output: Set(None),
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
    let build_id = new_build.id.clone().unwrap();

    let _ = tx.send(Action::Build(
        pkg.name,
        pkg.version,
        pkg.url_path.unwrap(),
        Box::from(pkg_model),
        Box::from(new_build),
    ));

    txn.commit().await?;

    Ok(build_id)
}
