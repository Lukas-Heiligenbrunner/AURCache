use crate::aur::api::get_package_info;
use crate::builder::types::{Action, BuildStates};
use crate::db::packages::{SourceData, SourceType};
use crate::db::prelude::Packages;
use crate::db::{builds, packages};
use anyhow::{anyhow, bail};
use pacman_mirrors::platforms::{Platform, Platforms};
use sea_orm::QueryFilter;
use sea_orm::{ActiveModelTrait, DatabaseConnection, EntityTrait, Set, TransactionTrait};
use sea_orm::{ColumnTrait, TryIntoModel};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::broadcast::Sender;

pub async fn package_add(
    db: &DatabaseConnection,
    pkg_name: String,
    tx: &Sender<Action>,
    platforms: Option<Vec<Platform>>,
    build_flags: Option<Vec<String>>,
    source_type: SourceType,
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

    let platforms_str = platforms
        .iter()
        .map(|platform| platform.as_str())
        .collect::<Vec<_>>()
        .join(";");

    let mut new_package = match source_type {
        SourceType::Aur => {
            let pkg = get_package_info(pkg_name)
                .await?
                .ok_or(anyhow!("Package not found"))?;

            let source_data = SourceData::Aur {};

            let new_package = packages::ActiveModel {
                name: Set(pkg_name.to_string()),
                status: Set(BuildStates::ENQUEUED_BUILD),
                version: Set(Some(pkg.version.clone())),
                latest_aur_version: Set(Option::from(pkg.version.clone())),
                platforms: Set(platforms_str),
                build_flags: Set(build_flags.join(";")),
                source_type: Set(source_type),
                source_data: Set(source_data.to_string()),
                ..Default::default()
            };
            new_package.save(db).await?
        }
        SourceType::Git => {
            let gitref = "#42";
            let source_data = SourceData::Git {
                // todo get real infos
                url: "".to_string(),
                r#ref: gitref.to_string(),
            };

            let new_package = packages::ActiveModel {
                name: Set(pkg_name.to_string()),
                status: Set(BuildStates::ENQUEUED_BUILD),
                version: Set(Some(gitref.to_string())),
                latest_aur_version: Set(Some(gitref.to_string())),
                platforms: Set(platforms_str),
                build_flags: Set(build_flags.join(";")),
                source_type: Set(source_type),
                source_data: Set(source_data.to_string()),
                ..Default::default()
            };
            new_package.save(db).await?
        }
        SourceType::Upload => {
            let source_data = SourceData::Upload {
                // todo get blob from upload
                archive: vec![],
            };

            // todo parse zip and its pkgbuild to get a version
            let version = "1.0.0";

            let new_package = packages::ActiveModel {
                name: Set(pkg_name.to_string()),
                status: Set(BuildStates::ENQUEUED_BUILD),
                // todo change to real versions
                version: Set(Some(version.to_string())),
                latest_aur_version: Set(Some(version.to_string())),
                platforms: Set(platforms_str),
                build_flags: Set(build_flags.join(";")),
                source_type: Set(source_type),
                source_data: Set(source_data.to_string()),
                ..Default::default()
            };
            new_package.save(db).await?
        }
    };

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
                    .expect("Duration must exist")
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

fn check_platforms(platforms: &Vec<Platform>) -> anyhow::Result<()> {
    for platform in platforms {
        if !Platforms.into_iter().any(|p| p == *platform) {
            bail!("Invalid platform: {}", platform);
        }
    }
    Ok(())
}
