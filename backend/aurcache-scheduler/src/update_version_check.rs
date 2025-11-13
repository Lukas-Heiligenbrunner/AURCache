use alpm_srcinfo::SourceInfoV1;
use anyhow::anyhow;
use aur_rs::{Package, Request};
use aurcache_builder::git::checkout::checkout_repo_ref;
use aurcache_db::helpers::active_value_ext::ActiveValueExt;
use aurcache_db::packages::{SourceData, SourceType};
use aurcache_db::prelude::{Builds, Packages};
use aurcache_db::{builds, packages};
use log::{error, info, warn};
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, DatabaseConnection, EntityTrait, Order, QuerySelect,
};
use sea_orm::{ColumnTrait, QueryFilter, QueryOrder};
use std::env;
use std::str::FromStr;
use std::time::Duration;
use tempfile::tempdir;
use tokio::{task::JoinHandle, time};

pub fn start_update_version_checking(db: DatabaseConnection) -> JoinHandle<()> {
    let default_version_check_interval = 3600;
    let check_interval = env::var("VERSION_CHECK_INTERVAL")
        .map(|x| x.parse::<u64>().unwrap_or(default_version_check_interval))
        .unwrap_or(default_version_check_interval);

    tokio::spawn(async move {
        let mut interval = time::interval(Duration::from_secs(check_interval));

        loop {
            interval.tick().await;
            info!("performing aur version checks");
            match check_versions(db.clone()).await {
                Ok(_) => {}
                Err(e) => {
                    error!("Failed to perform aur version check: {e}")
                }
            }
        }
    })
}

async fn check_versions(db: DatabaseConnection) -> anyhow::Result<()> {
    let packages = Packages::find().all(&db).await?;
    let aur_names: Vec<&str> = packages
        .iter()
        .filter(|x| x.source_type == SourceType::Aur)
        .map(|x| x.name.as_str())
        .collect();

    let results = if !aur_names.is_empty() {
        let request = Request::default();
        let response = request
            .search_multi_info_by_names(aur_names.as_slice())
            .await;

        let results: Vec<Package> = response
            .map_err(|_| anyhow!("couldn't download version update"))?
            .results;
        results
    } else {
        vec![]
    };

    if results.len() != aur_names.len() {
        warn!("Package nr in repo and aur api response has different size");
    }

    for package in packages {
        let mut package_model: packages::ActiveModel = package.clone().into();
        let package_id = package_model.id.get()?;

        // Query the latest build.version for this package (most recent by end_time then start_time)
        let latest_version_row = Builds::find()
            .select_only()
            .column(builds::Column::Version)
            .filter(builds::Column::PkgId.eq(*package_id))
            .order_by(builds::Column::EndTime, Order::Desc)
            .order_by(builds::Column::StartTime, Order::Desc)
            .limit(1)
            .into_tuple::<(String,)>()
            .one(&db)
            .await?;

        let latest_version: Option<String> = latest_version_row.map(|(v,)| v);

        let source_data = SourceData::from_str(package.source_data.as_str())?;
        match source_data {
            SourceData::Aur { .. } => match results.iter().find(|x1| x1.name == package.name) {
                None => {
                    warn!("Couldn't find {} in AUR response", package.name)
                }
                Some(result) => {
                    package_model.upstream_version = Set(Option::from(result.version.clone()));
                    package_model.out_of_date =
                        Set(if latest_version == Some(result.version.clone()) {
                            0
                        } else {
                            1
                        });
                }
            },
            SourceData::Git {
                url,
                subfolder,
                r#ref,
            } => {
                let dir = tempdir()?;
                let repo_path = dir.path().join("repo");

                checkout_repo_ref(url.to_string(), r#ref.to_string(), repo_path.clone())?;
                // todo maybe check also if latest commit hash changed

                let sourceinfo = SourceInfoV1::from_pkgbuild(
                    repo_path.join(subfolder).join("PKGBUILD").as_path(),
                )?;
                let version = sourceinfo.base.version.to_string();

                package_model.upstream_version = Set(Option::from(version.clone()));
                package_model.out_of_date = Set(if latest_version == Some(version) {
                    0
                } else {
                    1
                });

                _ = dir.close();
            }
            SourceData::Upload { .. } => {
                // noop since update is only triggered by new upload
            }
        }

        let _ = package_model.update(&db).await;
    }
    Ok(())
}
