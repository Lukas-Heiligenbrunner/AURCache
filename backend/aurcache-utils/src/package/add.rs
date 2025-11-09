use crate::aur::api::get_package_info;
use alpm_srcinfo::SourceInfoV1;
use anyhow::{anyhow, bail};
use aurcache_builder::git::checkout::checkout_repo_ref;
use aurcache_builder::types::{Action, BuildStates};
use aurcache_db::helpers::active_value_ext::ActiveValueExt;
use aurcache_db::packages::{SourceData, SourceType};
use aurcache_db::prelude::Packages;
use aurcache_db::{builds, packages};
use pacman_mirrors::platforms::{Platform, Platforms};
use sea_orm::QueryFilter;
use sea_orm::{ActiveModelTrait, DatabaseConnection, EntityTrait, Set, TransactionTrait};
use sea_orm::{ColumnTrait, TryIntoModel};
use std::time::{SystemTime, UNIX_EPOCH};
use tempfile::tempdir;
use tokio::sync::broadcast::Sender;

pub async fn package_add(
    db: &DatabaseConnection,
    tx: &Sender<Action>,
    platforms: Option<Vec<Platform>>,
    build_flags: Option<Vec<String>>,
    source_data: SourceData,
) -> anyhow::Result<String> {
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

    let source_type = match source_data {
        SourceData::Aur { .. } => SourceType::Aur,
        SourceData::Git { .. } => SourceType::Git,
        SourceData::Upload { .. } => SourceType::Upload,
    };

    let platforms_str = platforms
        .iter()
        .map(|platform| platform.as_str())
        .collect::<Vec<_>>()
        .join(";");

    let (mut new_package, new_version) = match source_data {
        SourceData::Aur { ref name } => {
            // remove leading and trailing whitespaces
            let pkg_name = name.trim();

            if Packages::find()
                .filter(packages::Column::Name.eq(pkg_name))
                .one(db)
                .await?
                .is_some()
            {
                bail!("Package already exists");
            }

            let pkg = get_package_info(pkg_name)
                .await?
                .ok_or(anyhow!("Package not found"))?;

            let new_package = packages::ActiveModel {
                name: Set(pkg_name.to_string()),
                status: Set(BuildStates::ENQUEUED_BUILD),
                latest_aur_version: Set(Option::from(pkg.version.clone())),
                platforms: Set(platforms_str),
                build_flags: Set(build_flags.join(";")),
                source_type: Set(source_type),
                source_data: Set(source_data.to_string()),
                ..Default::default()
            };
            (new_package.save(db).await?, pkg.version.clone())
        }
        SourceData::Git {
            ref r#ref,
            ref subfolder,
            ref url,
        } => {
            let dir = tempdir()?;
            let repo_path = dir.path().join("repo");

            // checkout repo to temp dir
            checkout_repo_ref(url.to_string(), r#ref.to_string(), repo_path.clone())?;

            // get package version from pkgbuild in subfolder
            let sourceinfo =
                SourceInfoV1::from_pkgbuild(repo_path.join(subfolder).join("PKGBUILD").as_path())?;
            let pkgbase_version = sourceinfo.base.version.to_string();
            let pkgbasee_name = sourceinfo.base.name;

            let pkg_name = pkgbasee_name.to_string();

            if Packages::find()
                .filter(packages::Column::Name.eq(pkg_name))
                .one(db)
                .await?
                .is_some()
            {
                bail!("Package already exists");
            }

            let new_package = packages::ActiveModel {
                name: Set(pkgbasee_name.to_string()),
                status: Set(BuildStates::ENQUEUED_BUILD),
                latest_aur_version: Set(Some(pkgbase_version)),
                platforms: Set(platforms_str),
                build_flags: Set(build_flags.join(";")),
                source_type: Set(source_type),
                source_data: Set(source_data.to_string()),
                ..Default::default()
            };
            (new_package.save(db).await?, r#ref.clone())
        }
        SourceData::Upload { .. } => {
            let source_data = SourceData::Upload {
                // todo get blob from upload
                archive: vec![],
            };

            // todo parse zip and its pkgbuild to get a version
            let version = "1.0.0";
            let name = "mypkg";

            let new_package = packages::ActiveModel {
                name: Set(name.to_string()),
                status: Set(BuildStates::ENQUEUED_BUILD),
                latest_aur_version: Set(Some(version.to_string())),
                platforms: Set(platforms_str),
                build_flags: Set(build_flags.join(";")),
                source_type: Set(source_type),
                source_data: Set(source_data.to_string()),
                ..Default::default()
            };
            (new_package.save(db).await?, version.to_string())
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
            version: Set(new_version.clone()),
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

    Ok(new_package.name.get()?.clone())
}

fn check_platforms(platforms: &Vec<Platform>) -> anyhow::Result<()> {
    for platform in platforms {
        if !Platforms.into_iter().any(|p| p == *platform) {
            bail!("Invalid platform: {}", platform);
        }
    }
    Ok(())
}
