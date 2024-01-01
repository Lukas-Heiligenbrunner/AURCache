use crate::db::packages;
use crate::db::prelude::Packages;
use anyhow::anyhow;
use aur_rs::{Package, Request};
use sea_orm::ActiveValue::Set;
use sea_orm::{ActiveModelTrait, DatabaseConnection, EntityTrait};
use std::time::Duration;
use tokio::time::sleep;

pub fn start_aur_version_checking(db: DatabaseConnection) {
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
            sleep(Duration::from_secs(3600)).await;
        }
    });
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

                package.latest_aur_version = Set(Some(result.version.clone()));
                let _ = package.update(&db).await;
            }
        }
    }
    Ok(())
}
