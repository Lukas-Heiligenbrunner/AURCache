use anyhow::anyhow;
use aurcache_db::dependencies;
use aurcache_db::prelude::{Builds, Dependencies, Packages};
use aurcache_db::{builds, packages};
use aurcache_types::builder::{Action, BuildStates};
use pacman_mirrors::platforms::Platform;
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, DatabaseConnection, EntityTrait,
    PaginatorTrait, QueryFilter, TransactionTrait, TryIntoModel,
};
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::broadcast::Sender;

pub async fn trigger_leaf_builds(
    db: &DatabaseConnection,
    tx: &Sender<Action>,
    platforms: &[Platform],
    pkgbases: &[String],
) -> anyhow::Result<()> {
    for pkgbase in pkgbases {
        let Some(pkg) = Packages::find()
            .filter(packages::Column::Pkgbase.eq(pkgbase))
            .one(db)
            .await?
        else {
            continue;
        };

        let dep_count = Dependencies::find()
            .filter(dependencies::Column::DependentId.eq(pkg.id))
            .count(db)
            .await?;

        if dep_count == 0 {
            let version = pkg
                .current_version
                .clone()
                .or(pkg.upstream_version.clone())
                .unwrap_or_default();
            trigger_build_for_package(db, tx, platforms, pkg, version).await?;
        }
    }
    Ok(())
}

pub async fn enqueue_missing_dependency_leaf_builds(
    db: &DatabaseConnection,
    tx: &Sender<Action>,
) -> anyhow::Result<usize> {
    let packages = Packages::find()
        .filter(packages::Column::DirectlyRequested.eq(false))
        .all(db)
        .await?;

    let mut queued = 0;
    for pkg in packages {
        let dep_count = Dependencies::find()
            .filter(dependencies::Column::DependentId.eq(pkg.id))
            .count(db)
            .await?;
        if dep_count != 0 {
            continue;
        }

        let build_count = Builds::find()
            .filter(builds::Column::PkgId.eq(pkg.id))
            .count(db)
            .await?;
        if build_count != 0 {
            continue;
        }

        let platforms = parse_platforms(&pkg.platforms)?;
        let version = pkg
            .current_version
            .clone()
            .or(pkg.upstream_version.clone())
            .unwrap_or_default();
        queued += trigger_build_for_package(db, tx, &platforms, pkg, version).await?;
    }

    Ok(queued)
}

async fn trigger_build_for_package(
    db: &DatabaseConnection,
    tx: &Sender<Action>,
    platforms: &[Platform],
    pkg: packages::Model,
    version: String,
) -> anyhow::Result<usize> {
    let mut queued = 0;

    for platform in platforms {
        let txn = db.begin().await?;

        let build = builds::ActiveModel {
            pkg_id: Set(pkg.id),
            output: Set(None),
            status: Set(Some(BuildStates::ENQUEUED_BUILD)),
            platform: Set(platform.to_string()),
            start_time: Set(Some(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .expect("Duration must exist")
                    .as_secs() as i64,
            )),
            version: Set(version.clone()),
            ..Default::default()
        };
        let new_build = build.save(&txn).await?;

        if *platform == Platform::X86_64 {
            let mut pkg_active: packages::ActiveModel = pkg.clone().into();
            pkg_active.latest_build = Set(Some(new_build.id.clone().unwrap()));
            pkg_active.save(&txn).await?;
        }

        txn.commit().await?;
        let _ = tx.send(Action::Build(
            Box::from(pkg.clone()),
            Box::from(new_build.try_into_model()?),
        ));
        queued += 1;
    }

    Ok(queued)
}

fn parse_platforms(platforms: &str) -> anyhow::Result<Vec<Platform>> {
    platforms
        .split(';')
        .filter(|platform| !platform.is_empty())
        .map(|platform| {
            Platform::from_str(platform)
                .map_err(|_| anyhow!("Invalid platform '{platform}' for queued dependency build"))
        })
        .collect()
}
