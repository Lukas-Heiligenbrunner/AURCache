use crate::db::migration::Order;
use crate::db::prelude::{Builds, Packages};
use crate::db::{builds, packages};
use crate::utils::db::ActiveValueExt;
use anyhow::anyhow;
use aur_rs::{Package, Request};
use log::{error, info, warn};
use sea_orm::{ActiveModelTrait, ActiveValue::Set, DatabaseConnection, EntityTrait, QuerySelect};
use sea_orm::{ColumnTrait, QueryFilter, QueryOrder};
use std::env;
use std::time::Duration;
use tokio::{task::JoinHandle, time};

pub fn start_aur_version_checking(db: DatabaseConnection) -> JoinHandle<()> {
    let default_version_check_interval = 3600;
    let check_interval = env::var("VERSION_CHECK_INTERVAL")
        .map(|x| x.parse::<u64>().unwrap_or(default_version_check_interval))
        .unwrap_or(default_version_check_interval);

    tokio::spawn(async move {
        let mut interval = time::interval(Duration::from_secs(check_interval));

        loop {
            interval.tick().await;
            info!("performing aur version checks");
            match aur_check_versions(db.clone()).await {
                Ok(_) => {}
                Err(e) => {
                    error!("Failed to perform aur version check: {e}")
                }
            }
        }
    })
}

async fn aur_check_versions(db: DatabaseConnection) -> anyhow::Result<()> {
    let packages = Packages::find().all(&db).await?;
    let names: Vec<&str> = packages.iter().map(|x| x.name.as_str()).collect();

    let request = Request::default();
    let response = request.search_multi_info_by_names(names.as_slice()).await;

    let results: Vec<Package> = response
        .map_err(|_| anyhow!("couldn't download version update"))?
        .results;

    if results.len() != packages.len() {
        warn!("Package nr in repo and aur api response has different size");
    }

    for package in packages {
        match results.iter().find(|x1| x1.name == package.name) {
            None => {
                warn!("Couldn't find {} in AUR response", package.name)
            }
            Some(result) => {
                let mut package: packages::ActiveModel = package.into();
                let package_id = package.id.get()?;

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

                package.latest_aur_version = Set(Option::from(result.version.clone()));
                package.out_of_date = Set(if latest_version == Some(result.version.clone()) {
                    0
                } else {
                    1
                });
                let _ = package.update(&db).await;
            }
        }
    }
    Ok(())
}
