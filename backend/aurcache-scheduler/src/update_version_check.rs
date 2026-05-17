use anyhow::anyhow;
use aurcache_db::helpers::active_value_ext::ActiveValueExt;
use aurcache_db::packages::{SourceData, SourceType};
use aurcache_db::prelude::{Builds, Packages};
use aurcache_db::{builds, packages};
use aurcache_deps::AurClient;
use aurcache_types::settings::{ApplicationSettings, Setting, SettingsEntry};
use aurcache_utils::git::sourceinfo::load_git_sourceinfo;
use aurcache_utils::settings::general::SettingsTraits;
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, DatabaseConnection, EntityTrait, Order, QuerySelect,
};
use sea_orm::{ColumnTrait, QueryFilter, QueryOrder};
use std::str::FromStr;
use std::time::Duration;
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

#[must_use]
pub fn start_update_version_checking(db: DatabaseConnection) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            info!("performing aur version checks");
            if let Err(e) = check_versions(db.clone()).await {
                error!("Failed to perform aur version check: {e}");
            }

            let check_interval: SettingsEntry<u64> =
                ApplicationSettings::get(Setting::VersionCheckInterval, None, &db).await;
            tokio::time::sleep(Duration::from_secs(check_interval.value)).await;
        }
    })
}

async fn check_versions(db: DatabaseConnection) -> anyhow::Result<()> {
    let packages = Packages::find().all(&db).await?;
    let aur_names: Vec<&str> = packages
        .iter()
        .filter(|x| x.source_type == SourceType::Aur)
        .map(|x| x.pkgbase.as_str())
        .collect();

    let results = if aur_names.is_empty() {
        vec![]
    } else {
        let client = AurClient::new();
        client
            .multi_info_of(&aur_names)
            .await
            .map_err(|_| anyhow!("couldn't download version update"))?
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
            SourceData::Aur { .. } => {
                match results.iter().find(|x1| x1.package_base == package.pkgbase) {
                    None => {
                        warn!("Couldn't find {} in AUR response", package.pkgbase);
                    }
                    Some(result) => {
                        package_model.upstream_version = Set(Option::from(result.version.clone()));
                        package_model.out_of_date =
                            Set(i32::from(latest_version != Some(result.version.clone())));
                    }
                }
            }
            SourceData::Git { spec } => {
                let sourceinfo = load_git_sourceinfo(&spec)?;
                let version = sourceinfo.base.version.to_string();

                package_model.upstream_version = Set(Option::from(version.clone()));
                package_model.out_of_date = Set(i32::from(latest_version != Some(version)));
            }
            SourceData::Upload { .. } => {
                // noop since update is only triggered by new upload
            }
        }

        let _ = package_model.update(&db).await;
    }
    Ok(())
}
