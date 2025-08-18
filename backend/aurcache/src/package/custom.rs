use crate::builder::types::{Action, BuildStates};
use crate::db::prelude::Packages;
use crate::db::{builds, packages};
use crate::package::types::PackageType;
use crate::utils::db::ActiveValueExt;
use anyhow::{anyhow, bail};
use pacman_mirrors::platforms::{Platform, Platforms};
use sea_orm::QueryFilter;
use sea_orm::{ActiveModelTrait, DatabaseConnection, EntityTrait, Set, TransactionTrait};
use sea_orm::{ColumnTrait, TryIntoModel};
use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::broadcast::Sender;

pub async fn package_add_custom(
    db: &DatabaseConnection,
    pkg_name: String,
    pkgbuild_content: String,
    version: String,
    tx: &Sender<Action>,
    platforms: Option<Vec<Platform>>,
    build_flags: Option<Vec<String>>,
) -> anyhow::Result<()> {
    let platforms = match platforms {
        None => vec![Platform::X86_64],
        Some(platforms) => {
            check_platforms(&platforms)?;
            platforms
        }
    };
    let build_flags = build_flags.unwrap_or_else(|| {
        vec![
            "-Syu".to_string(),
            "--noconfirm".to_string(),
            "--noprogressbar".to_string(),
            "--color never".to_string(),
        ]
    });

    // remove leading and trailing whitespaces
    let pkg_name = pkg_name.trim();

    if Packages::find()
        .filter(packages::Column::Name.eq(pkg_name))
        .one(db)
        .await?
        .is_some()
    {
        bail!("Package already exists");
    }

    // Create custom packages directory if it doesn't exist
    let custom_pkgbuild_dir = "./custom_packages";
    fs::create_dir_all(custom_pkgbuild_dir)?;

    // Save PKGBUILD file
    let pkgbuild_path = format!("{}/{}/PKGBUILD", custom_pkgbuild_dir, pkg_name);
    let pkg_dir = format!("{}/{}", custom_pkgbuild_dir, pkg_name);
    fs::create_dir_all(&pkg_dir)?;
    fs::write(&pkgbuild_path, pkgbuild_content)?;

    let new_package = packages::ActiveModel {
        name: Set(pkg_name.to_string()),
        status: Set(BuildStates::ENQUEUED_BUILD),
        version: Set(Some(version)),
        latest_aur_version: Set(None), // No AUR version for custom packages
        platforms: Set(platforms
            .iter()
            .map(|platform| platform.as_str())
            .collect::<Vec<_>>()
            .join(";")),
        build_flags: Set(build_flags.join(";")),
        package_type: Set(PackageType::Custom as i32),
        custom_pkgbuild_path: Set(Some(pkgbuild_path)),
        ..Default::default()
    };
    let mut new_package = new_package.save(db).await?;

    // trigger new build for each platform
    for platform in platforms {
        let txn = db.begin().await?;

        // set build status to pending
        let build = builds::ActiveModel {
            pkg_id: new_package.id.clone(),
            output: Set(None),
            status: Set(Some(BuildStates::ENQUEUED_BUILD)),
            // todo add new column for enqueued_time
            platform: Set(platform.to_string()),
            start_time: Set(Some(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs() as i64,
            )),
            ..Default::default()
        };
        let new_build = build.save(&txn).await?;

        // todo -- setting latest build to latest x86_64 build for now
        if platform == Platform::X86_64 {
            new_package.latest_build = Set(Some(new_build.id.clone().unwrap()));
            new_package = new_package.save(&txn).await?;
        }

        txn.commit().await?;
        let _ = tx.send(Action::Build(
            Box::from(new_package.clone().try_into_model()?),
            Box::from(new_build.try_into_model()?),
        ));
    }

    Ok(())
}

pub async fn package_update_custom(
    db: &DatabaseConnection,
    pkg_id: i32,
    pkgbuild_content: String,
    version: String,
    tx: &Sender<Action>,
) -> anyhow::Result<Vec<i32>> {
    let pkg_model: packages::Model = Packages::find_by_id(pkg_id)
        .one(db)
        .await?
        .ok_or(anyhow!("Package not found"))?;

    if pkg_model.package_type != (PackageType::Custom as i32) {
        bail!("Package is not a custom package");
    }

    // Update PKGBUILD file
    if let Some(ref pkgbuild_path) = pkg_model.custom_pkgbuild_path {
        fs::write(pkgbuild_path, pkgbuild_content)?;
    } else {
        bail!("Custom package has no PKGBUILD path");
    }

    let txn = db.begin().await?;

    let mut pkg_model_active: packages::ActiveModel = pkg_model.clone().into();
    pkg_model_active.status = Set(BuildStates::ENQUEUED_BUILD);
    pkg_model_active.version = Set(Some(version));
    let pkg_active_model = pkg_model_active.save(&txn).await?;
    txn.commit().await?;

    let mut build_ids = vec![];

    let pkg_model: packages::Model = pkg_active_model.clone().try_into()?;
    for platform in pkg_model.platforms.clone().split(";") {
        let build_id = update_platform(platform, pkg_model.clone(), db, tx).await?;
        build_ids.push(build_id);
    }

    Ok(build_ids)
}

async fn update_platform(
    platform: &str,
    pkg_model: packages::Model,
    db: &DatabaseConnection,
    tx: &Sender<Action>,
) -> anyhow::Result<i32> {
    let txn = db.begin().await?;

    let build = builds::ActiveModel {
        pkg_id: Set(pkg_model.id),
        output: Set(None),
        status: Set(Some(BuildStates::ENQUEUED_BUILD)),
        platform: Set(platform.to_string()),
        start_time: Set(Some(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64,
        )),
        ..Default::default()
    };

    let new_build = build.save(&txn).await?;
    txn.commit().await?;

    let _ = tx.send(Action::Build(
        Box::from(pkg_model),
        Box::from(new_build.clone().try_into_model()?),
    ));

    Ok(*new_build.id.get()?)
}

fn check_platforms(platforms: &Vec<Platform>) -> anyhow::Result<()> {
    for platform in platforms {
        if !Platforms.into_iter().any(|p| p == *platform) {
            bail!("Invalid platform: {}", platform);
        }
    }
    Ok(())
}