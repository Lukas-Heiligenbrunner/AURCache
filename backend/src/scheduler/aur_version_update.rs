use crate::db::packages;
use crate::db::prelude::{Packages};
use anyhow::anyhow;
use aur_rs::{Package, Request};
use sea_orm::ActiveValue::Set;
use sea_orm::{ActiveModelTrait, DatabaseConnection, EntityTrait};
use std::env;
use std::time::Duration;
use tokio::task::JoinHandle;
use tokio::time::sleep;

pub fn start_aur_version_checking(db: DatabaseConnection) -> JoinHandle<()> {
    let default_version_check_interval = 3600;
    let check_interval = env::var("VERSION_CHECK_INTERVAL")
        .map(|x| x.parse::<u64>().unwrap_or(default_version_check_interval))
        .unwrap_or(default_version_check_interval);

    tokio::spawn(async move {
        sleep(Duration::from_secs(10)).await;
        loop {
            println!("performing aur version checks");
            match aur_check_versions(db.clone()).await {
                Ok(_) => {}
                Err(e) => {
                    println!("Failed to perform aur version check: {e}")
                }
            }
            sleep(Duration::from_secs(check_interval)).await;
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
        println!("Package nr in repo and aur api response has different size");
    }

    for package in packages {
        match results.iter().find(|x1| x1.name == package.name) {
            None => {
                println!("Couldn't find {} in AUR response", package.name)
            }
            Some(result) => {
                let mut package: packages::ActiveModel = package.into();
                let latest_version = package.version.clone().unwrap();

                package.latest_aur_version = Set(Option::from(result.version.clone()));
                package.out_of_date = Set(if latest_version == result.version { 0 } else { 1 });
                let _ = package.update(&db).await;
            }
        }
    }
    Ok(())
}
