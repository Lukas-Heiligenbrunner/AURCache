use anyhow::anyhow;
use aurcache_db::helpers::active_value_ext::ActiveValueExt;
use aurcache_db::packages::{SourceData, SourceType};
use aurcache_db::prelude::{Builds, Packages};
use aurcache_db::{builds, packages};
use aurcache_deps::AurClient;
use aurcache_types::settings::{ApplicationSettings, Setting, SettingsEntry};
use aurcache_utils::pkg::vercmp;
use aurcache_utils::settings::general::SettingsTraits;
use aurcache_utils::snapshot::SnapshotStore;
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, DatabaseConnection, EntityTrait, Order, QuerySelect,
};
use sea_orm::{ColumnTrait, QueryFilter, QueryOrder};
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

#[must_use]
pub fn start_update_version_checking(
    db: DatabaseConnection,
    store: Arc<SnapshotStore>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            info!("performing aur version checks");
            if let Err(e) = check_versions(db.clone(), &store).await {
                error!("Failed to perform aur version check: {e}");
            }

            let check_interval: SettingsEntry<u64> =
                ApplicationSettings::get(Setting::VersionCheckInterval, None, &db).await;
            tokio::time::sleep(Duration::from_secs(check_interval.value)).await;
        }
    })
}

async fn check_versions(db: DatabaseConnection, store: &SnapshotStore) -> anyhow::Result<()> {
    let packages = Packages::find().all(&db).await?;
    let client = AurClient::new();
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
                        // Only mark out of date when upstream is strictly newer than the
                        // locally built version.  This prevents VCS packages (-git etc.)
                        // from looping: the AUR-reported version is the one from when the
                        // PKGBUILD was last touched, which may be *older* than what was
                        // actually built from the live VCS source.
                        let is_outdated = match &latest_version {
                            None => true,
                            Some(built) => {
                                vercmp(&result.version, built) == std::cmp::Ordering::Greater
                            }
                        };
                        package_model.out_of_date = Set(i32::from(is_outdated));

                        // The AUR RPC `/info` response is a cheap way to know
                        // whether the package has actually changed upstream
                        // (via `version`/`last_modified`); only refresh the
                        // (git-backed) snapshot cache -- which requires a
                        // `git fetch` -- when it looks like something changed,
                        // instead of unconditionally re-fetching every package
                        // on every check.
                        if is_outdated && let Err(e) = store.refresh(&client, &source_data).await {
                            warn!("Failed to refresh snapshot cache for {}: {e}", package.name);
                        }
                    }
                }
            }
            SourceData::Git { .. } => {
                // No cheap upstream-metadata API for arbitrary git remotes,
                // so always refresh: this is an incremental `git fetch`
                // against the persistent checkout, not a full re-clone.
                // A failure here must not abort version-checking for the
                // remaining packages: one unreachable git remote would
                // otherwise leave every package after it in this run without
                // an updated out-of-date flag. It only means this package's
                // own version tracking cannot be refreshed this round; a real
                // problem with the source surfaces again, per-package, when
                // its build runs. The AUR branch above already isolates
                // failures this way.
                if let Err(e) = store.refresh(&client, &source_data).await {
                    warn!("Failed to refresh git source for {}: {e}", package.name);
                    let _ = package_model.update(&db).await;
                    continue;
                }
                let sourceinfo = match store.sourceinfo(&client, &source_data).await {
                    Ok(sourceinfo) => sourceinfo,
                    Err(e) => {
                        warn!("Failed to get sourceinfo for {}: {e}", package.name);
                        let _ = package_model.update(&db).await;
                        continue;
                    }
                };
                // This still only tracks the version in PKGBUILD/.SRCINFO; a ref
                // moving without a version bump will not mark the package outdated.
                let version = sourceinfo.base.version.to_string();

                package_model.upstream_version = Set(Option::from(version.clone()));
                // Same logic as for AUR packages: only mark out of date when the
                // upstream PKGBUILD version is strictly newer than what was built.
                let is_outdated = match &latest_version {
                    None => true,
                    Some(built) => vercmp(&version, built) == std::cmp::Ordering::Greater,
                };
                package_model.out_of_date = Set(i32::from(is_outdated));
            }
            SourceData::Upload { .. } => {
                // noop since update is only triggered by new upload
            }
        }

        let _ = package_model.update(&db).await;
    }
    Ok(())
}
