use crate::aur::api::get_info_by_name;
use crate::builder::types::{Action, BuildStates};
use crate::db::prelude::Packages;
use crate::db::{builds, packages};
use crate::repo::platforms::PLATFORMS;
use anyhow::bail;
use sea_orm::ColumnTrait;
use sea_orm::QueryFilter;
use sea_orm::{ActiveModelTrait, DatabaseConnection, EntityTrait, Set, TransactionTrait};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::broadcast::Sender;

pub async fn package_add(
    db: &DatabaseConnection,
    pkg_name: String,
    tx: &Sender<Action>,
    platforms: Option<Vec<String>>,
    build_flags: Option<Vec<String>>,
) -> anyhow::Result<()> {
    let platforms = match platforms {
        None => vec!["x86_64".to_string()],
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

    let pkg = get_info_by_name(pkg_name).await?;

    let new_package = packages::ActiveModel {
        name: Set(pkg_name.to_string()),
        status: Set(BuildStates::ENQUEUED_BUILD),
        version: Set(Some(pkg.version.clone())),
        latest_aur_version: Set(Option::from(pkg.version.clone())),
        platforms: Set(platforms.join(";")),
        build_flags: Set(build_flags.join(";")),
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
            platform: Set(platform.clone()),
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
        if platform == "x86_64" {
            new_package.latest_build = Set(Some(new_build.id.clone().unwrap()));
            new_package = new_package.save(&txn).await?;
        }

        txn.commit().await?;
        let _ = tx.send(Action::Build(
            Box::from(new_package.clone()),
            Box::from(new_build),
        ));
    }

    Ok(())
}

fn check_platforms(platforms: &Vec<String>) -> anyhow::Result<()> {
    for platform in platforms {
        if !PLATFORMS.contains(&platform.as_str()) {
            bail!("Invalid platform: {}", platform);
        }
    }
    Ok(())
}
