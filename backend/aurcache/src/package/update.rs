use crate::activity_log::activity_utils::ActivityLog;
use crate::activity_log::package_update_activity::PackageUpdateActivity;
use crate::aur::api::get_info_by_name;
use crate::builder::types::{Action, BuildStates};
use crate::db::activities::ActivityType;
use crate::db::prelude::Packages;
use crate::db::{builds, packages};
use anyhow::{anyhow, bail};
use log::warn;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, Set,
    TransactionTrait, TryIntoModel,
};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::broadcast::Sender;

/// Updates all outdated packages in the database.
///
/// This function queries the database for packages marked as outdated and updates them.
///
/// # Arguments
///
/// * `db` - A reference to the database connection.
/// * `tx` - A broadcast channel sender for triggering build actions.
///
/// # Returns
///
/// * `Ok(Vec<i32>)` - A vector of build IDs that were updated.
/// * `Err(anyhow::Error)` - If any error occurs during the update process.
pub async fn package_update_all_outdated(
    db: &DatabaseConnection,
    tx: &Sender<Action>,
) -> anyhow::Result<Vec<i32>> {
    let txn = db.begin().await?;

    let pkg_models: Vec<packages::Model> = Packages::find()
        .filter(packages::Column::OutOfDate.eq(1))
        .all(&txn)
        .await?;
    let activity_log = ActivityLog::new(db.clone());

    let mut ids_total = vec![];
    for pkg in pkg_models.iter() {
        // only trigger build if previous build was successful and no build active
        if pkg.status == BuildStates::SUCCESSFUL_BUILD {
            let mut ids = package_update(db, pkg.to_owned(), false, tx).await?;
            activity_log
                .add(
                    PackageUpdateActivity {
                        package: pkg.name.clone(),
                        forced: false,
                    },
                    ActivityType::UpdatePackage,
                    Some("Server".to_string()),
                )
                .await?;
            ids_total.append(&mut ids);
        } else {
            warn!(
                "Package auto update was not triggered for package {} because of prev. build status: {}",
                pkg.name, pkg.status
            );
        }
    }
    Ok(ids_total)
}

/// Updates a single package for all required platforms.
///
/// This function fetches the latest package metadata and updates it if necessary.
///
/// # Arguments
///
/// * `db` - A reference to the database connection.
/// * `pkg_model` - The package model to update.
/// * `force` - A boolean flag to force an update even if the package version is unchanged.
/// * `tx` - A broadcast channel sender for triggering build actions.
///
/// # Returns
///
/// * `Ok(Vec<i32>)` - A vector of build IDs for the updated package.
/// * `Err(anyhow::Error)` - If any error occurs during the update trigger.
pub async fn package_update(
    db: &DatabaseConnection,
    pkg_model: packages::Model,
    force: bool,
    tx: &Sender<Action>,
) -> anyhow::Result<Vec<i32>> {
    let txn = db.begin().await?;

    let mut pkg_model_active: packages::ActiveModel = pkg_model.clone().into();

    let pkg = get_info_by_name(pkg_model.name.as_str())
        .await
        .map_err(|_| anyhow!("couldn't download package metadata".to_string()))?;

    if !force && pkg_model.version == Some(pkg.version.clone()) {
        bail!("Package is already up to date");
    }

    pkg_model_active.status = Set(BuildStates::ENQUEUED_BUILD);
    pkg_model_active.version = Set(Some(pkg.version.clone()));
    let pkg_aktive_model = pkg_model_active.save(&txn).await?;
    txn.commit().await?;

    let mut build_ids = vec![];

    let pkg_model: packages::Model = pkg_aktive_model.clone().try_into()?;
    for platform in pkg_model.platforms.clone().split(";") {
        let build_id = update_platform(platform, pkg_model.clone(), db, tx).await?;
        build_ids.push(build_id);
    }

    Ok(build_ids)
}

/// Creates a build entry for a package on a specific platform.
///
/// This function initializes a new build job in the database and triggers the build process.
///
/// # Arguments
///
/// * `platform` - The platform on which the package should be built.
/// * `pkg` - The package model associated with the build.
/// * `db` - A reference to the database connection.
/// * `tx` - A broadcast channel sender for triggering build actions.
///
/// # Returns
///
/// * `Ok(i32)` - The ID of the created build entry.
/// * `Err(anyhow::Error)` - If any error occurs during the build process.
pub async fn update_platform(
    platform: &str,
    pkg: packages::Model,
    db: &DatabaseConnection,
    tx: &Sender<Action>,
) -> anyhow::Result<i32> {
    let txn = db.begin().await?;
    // set build status to pending
    let start_time = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
    let build = builds::ActiveModel {
        pkg_id: Set(pkg.id),
        output: Set(None),
        status: Set(Some(BuildStates::ENQUEUED_BUILD)),
        start_time: Set(Some(start_time)),
        platform: Set(platform.to_string()),
        ..Default::default()
    };
    let new_build = build.save(&txn).await?;
    let build_id = new_build.id.clone().unwrap();
    txn.commit().await?;

    let _ = tx.send(Action::Build(
        Box::from(pkg),
        Box::from(new_build.try_into_model()?),
    ));
    Ok(build_id)
}
