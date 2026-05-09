use anyhow::anyhow;
use aurcache_db::helpers::active_value_ext::ActiveValueExt;
use aurcache_db::packages::{SourceData, SourceType};
use aurcache_db::prelude::{Builds, Packages};
use aurcache_db::{builds, packages};
use aurcache_deps::AurClient;
use aurcache_types::settings::{ApplicationSettings, Setting, SettingsEntry};
use aurcache_utils::settings::general::SettingsTraits;
use aurcache_utils::snapshot::SnapshotStore;
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, DatabaseConnection, EntityTrait, Order, QuerySelect,
};
use sea_orm::{ColumnTrait, QueryFilter, QueryOrder};
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
    let client = AurClient::new();
    let store = SnapshotStore::new();
    let aur_query_names: Vec<String> = packages
        .iter()
        .filter(|x| x.source_type == SourceType::Aur)
        .map(|x| {
            // AUR RPC /info matches by pkgname, not pkgbase.  For packages whose
            // pkgbase differs from any child pkgname (e.g. czkawka → czkawka-cli),
            // query by the first split child so the RPC returns a result whose
            // package_base field can be matched below.
            x.split_packages
                .as_deref()
                .and_then(|sp| serde_json::from_str::<Vec<String>>(sp).ok())
                .filter(|names| names.len() > 1)
                .and_then(|names| names.first().cloned())
                .unwrap_or_else(|| x.name.clone())
        })
        .collect();

    let aur_name_refs: Vec<&str> = aur_query_names.iter().map(|s| s.as_str()).collect();

    let results = if aur_name_refs.is_empty() {
        vec![]
    } else {
        client
            .multi_info_of(&aur_name_refs)
            .await
            .map_err(|_| anyhow!("couldn't download version update"))?
    };

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

        let source_data = package.source_data;
        match source_data {
            SourceData::Aur { .. } => {
                match results.iter().find(|x1| x1.package_base == package.name) {
                    None => {
                        warn!("Couldn't find {} in AUR response", package.name);
                    }
                    Some(result) => {
                        package_model.upstream_version = Set(Option::from(result.version.clone()));
                        package_model.out_of_date =
                            Set(i32::from(latest_version != Some(result.version.clone())));
                    }
                }
            }
            SourceData::Git { .. } => {
                let sourceinfo = store
                    .sourceinfo(&client, &source_data)
                    .await
                    .map_err(|e| anyhow!("Failed to get sourceinfo: {e}"))?;
                // This still only tracks the version in PKGBUILD/.SRCINFO; a ref
                // moving without a version bump will not mark the package outdated.
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
