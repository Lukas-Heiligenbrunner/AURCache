use crate::aur::aur::get_info_by_name;
use crate::builder::types::Action;
use crate::db::prelude::{Packages, Versions};
use crate::db::{builds, packages, versions};
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

    let version_model = match Versions::find()
        .filter(versions::Column::Version.eq(pkg.version.clone()))
        .filter(versions::Column::PackageId.eq(pkg.id.clone()))
        .one(&txn)
        .await?
    {
        None => {
            let new_version = versions::ActiveModel {
                version: Set(pkg.version.clone()),
                package_id: Set(pkg_model.id.clone().unwrap()),
                ..Default::default()
            };

            new_version.save(&txn).await.expect("TODO: panic message")
        }
        Some(p) => {
            // todo add check if this version was successfully built
            // if not allow build
            if force {
                p.into()
            } else {
                return Err(anyhow!("Version already existing"));
            }
        }
    };

    pkg_model.status = Set(3);
    pkg_model.latest_version_id = Set(Some(version_model.id.clone().unwrap()));
    pkg_model.save(&txn).await?;

    // set build status to pending
    let build = builds::ActiveModel {
        pkg_id: version_model.package_id.clone(),
        version_id: version_model.id.clone(),
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
    let build_id = new_build.id.clone().unwrap();

    let _ = tx.send(Action::Build(
        pkg.name,
        pkg.version,
        pkg.url_path.unwrap(),
        version_model,
        new_build,
    ));

    txn.commit().await?;

    Ok(build_id)
}
