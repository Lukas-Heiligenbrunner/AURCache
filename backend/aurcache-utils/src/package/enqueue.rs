use anyhow::anyhow;
use aurcache_db::dependencies;
use aurcache_db::helpers::build_enqueue::enqueue_build_if_missing;
use aurcache_db::prelude::{Builds, Dependencies, Packages};
use aurcache_db::{builds, packages};
use aurcache_types::builder::{Action, BuildStates};
use futures::future::try_join_all;
use pacman_mirrors::platforms::Platform;
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, DatabaseConnection, EntityTrait,
    PaginatorTrait, QueryFilter, QueryOrder, QuerySelect, TransactionTrait,
};
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::broadcast::Sender;
use tracing::warn;

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

pub async fn enqueue_missing_buildable_packages(
    db: &DatabaseConnection,
    tx: &Sender<Action>,
) -> anyhow::Result<usize> {
    let packages = Packages::find().all(db).await?;

    let mut queued = 0;
    for pkg in packages {
        let platforms = match parse_platforms(&pkg.platforms) {
            Ok(platforms) => platforms,
            Err(error) => {
                warn!(
                    "Skipping package {} during startup enqueue because platforms are invalid: {error}",
                    pkg.pkgbase
                );
                continue;
            }
        };
        let ready_platforms = try_join_all(platforms.into_iter().map(|platform| async move {
            let platform_name = platform.to_string();
            let ready = !build_exists_for_platform(db, pkg.id, &platform_name).await?
                && dependencies_satisfied(db, pkg.id, &platform_name).await?;
            Ok::<_, anyhow::Error>(ready.then_some(platform))
        }))
        .await?
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
        if ready_platforms.is_empty() {
            continue;
        }

        let version = pkg
            .current_version
            .clone()
            .or(pkg.upstream_version.clone())
            .unwrap_or_default();
        queued += trigger_build_for_package(db, tx, &ready_platforms, pkg, version).await?;
    }

    Ok(queued)
}

async fn build_exists_for_platform(
    db: &DatabaseConnection,
    pkg_id: i32,
    platform: &str,
) -> anyhow::Result<bool> {
    Ok(Builds::find()
        .filter(builds::Column::PkgId.eq(pkg_id))
        .filter(builds::Column::Platform.eq(platform))
        .count(db)
        .await?
        != 0)
}

async fn dependencies_satisfied(
    db: &DatabaseConnection,
    dependent_id: i32,
    platform: &str,
) -> anyhow::Result<bool> {
    let deps = Dependencies::find()
        .filter(dependencies::Column::DependentId.eq(dependent_id))
        .all(db)
        .await?;

    for dep in deps {
        let Some((version,)) = Builds::find()
            .select_only()
            .column(builds::Column::Version)
            .filter(builds::Column::PkgId.eq(dep.dependee_id))
            .filter(builds::Column::Platform.eq(platform))
            .filter(builds::Column::Status.eq(Some(BuildStates::SUCCESSFUL_BUILD)))
            .order_by_desc(builds::Column::EndTime)
            .order_by_desc(builds::Column::StartTime)
            .into_tuple::<(String,)>()
            .one(db)
            .await?
        else {
            return Ok(false);
        };

        if !crate::pkg::satisfies_constraint(&version, &dep.version_constraint) {
            return Ok(false);
        }
    }

    Ok(true)
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
        let enqueue_result = enqueue_build_if_missing(
            &txn,
            pkg.id,
            &platform.to_string(),
            &version,
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("Duration must exist")
                .as_secs() as i64,
        )
        .await?;

        if *platform == Platform::X86_64 {
            let mut pkg_active: packages::ActiveModel = pkg.clone().into();
            pkg_active.latest_build = Set(Some(enqueue_result.build.id));
            pkg_active.save(&txn).await?;
        }

        txn.commit().await?;
        if enqueue_result.inserted {
            let _ = tx.send(Action::Build(
                Box::from(pkg.clone()),
                Box::from(enqueue_result.build),
            ));
            queued += 1;
        }
    }

    Ok(queued)
}

fn parse_platforms(platforms: &str) -> anyhow::Result<Vec<Platform>> {
    crate::platforms::split_platform_list(platforms)
        .into_iter()
        .map(|platform| {
            Platform::from_str(platform)
                .map_err(|_| anyhow!("Invalid platform '{platform}' for queued dependency build"))
        })
        .collect()
}
