use anyhow::anyhow;
use aurcache_activitylog::activity_utils::ActivityLog;
use aurcache_activitylog::events::{Event, RefreshTarget, SourceinfoPurpose};
use aurcache_db::helpers::builds::latest_successful_version_any_platform;
use aurcache_db::packages;
use aurcache_db::packages::SourceData;
use aurcache_db::prelude::Packages;
use aurcache_utils::package::metadata::apply_source_metadata;
use aurcache_utils::package::update::package_update_all_outdated;
use aurcache_utils::pkg::vercmp;
use aurcache_utils::services::Services;
use aurcache_utils::settings;
use aurcache_utils::snapshot::Resolved;
use aurcache_utils::vcs_check::{RoundCache, VcsSync, sync_vcs_sources};
use sea_orm::{ActiveModelTrait, ActiveValue::Set, DatabaseConnection, EntityTrait};
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use tokio::task::JoinHandle;
use tracing::{error, info};

#[must_use]
pub fn start_update_version_checking(services: Services) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut unreadable = Unreadable::default();
        loop {
            info!("performing aur version checks");
            if let Err(e) = check_versions(&services, &mut unreadable).await {
                error!("Failed to perform aur version check: {e}");
                // Nothing was found to be out of date this pass, which looks
                // exactly like nothing *being* out of date unless it says so.
                services.activity.emit(Event::VersionCheckFailed {
                    error: format!("{e:#}"),
                });
            }

            let check_interval =
                settings::get(&services.db, settings::key::VERSION_CHECK_INTERVAL, None).await;
            let delay = match check_interval {
                Ok(interval) => interval.value.0.max(1),
                Err(error) => {
                    error!("could not read version-check interval: {error}");
                    30
                }
            };
            tokio::time::sleep(Duration::from_secs(delay)).await;
        }
    })
}

/// Packages whose checkout could not be read, already reported.
///
/// The check runs every hour, and the same failure every hour is not news: a
/// dozen packages that will not parse filled the log with hundreds of
/// identical warnings. Kept across rounds, and forgotten once a package reads
/// again, so a later failure is reported afresh.
#[derive(Default)]
struct Unreadable(HashSet<i32>);

/// Choose a produced package name for AUR RPC, which cannot look up a pkgbase.
fn aur_query_name(pkgbase: &str, split_packages: Option<&str>) -> String {
    aurcache_db::lists::json_list(split_packages)
        .into_iter()
        .next()
        .unwrap_or_else(|| pkgbase.to_string())
}

async fn check_versions(services: &Services, unreadable: &mut Unreadable) -> anyhow::Result<()> {
    let Services {
        db,
        store,
        client,
        activity,
        ..
    } = services;
    let packages = Packages::find().all(db).await?;
    let aur_query_names: Vec<String> = packages
        .iter()
        .filter(|x| matches!(x.source_data, SourceData::Aur { .. }))
        .map(|package| aur_query_name(&package.name, package.split_packages.as_deref()))
        .collect();

    let aur_name_refs: Vec<&str> = aur_query_names.iter().map(String::as_str).collect();

    let results = if aur_name_refs.is_empty() {
        Vec::new()
    } else {
        client
            .multi_info_of(&aur_name_refs)
            .await
            .map_err(|e| anyhow!("couldn't download version update: {e}"))?
    };
    // Indexed once: the per-package loop below looks every package up by its
    // pkgbase, which a linear scan would make quadratic in the package count.
    let by_base: HashMap<&str, _> = results
        .iter()
        .map(|result| (result.package_base.as_str(), result))
        .collect();

    // One pass, one answer per remote: several packages can name the same
    // upstream, and what its ref points at does not change between two of them.
    let mut round = RoundCache::new();

    for package in packages {
        // Only what this sweep sets: the loop does network I/O per package
        // (AUR RPC, git fetches), so by the time a late package saves, a row
        // read at the top is minutes stale -- and a full-row `update()` would
        // write that staleness back over every column, including ones a
        // concurrently finishing build just changed. A partial update touches
        // the sweep's own columns and nothing else.
        let mut model = packages::ActiveModel {
            id: Set(package.id),
            ..Default::default()
        };
        let check = Check {
            services,
            package: &package,
        };
        match &package.source_data {
            SourceData::Aur { .. } => {
                let Some(result) = by_base.get(package.name.as_str()) else {
                    // Removed from the AUR. Recorded so the package page can
                    // say so: its metadata still comes from the checkout,
                    // which continues to exist.
                    activity.emit(Event::AurMissing {
                        pkg: package.name.as_str().into(),
                    });
                    model.aur_missing = Set(Some(true));
                    save_package(db, activity, model, &package.name).await;
                    continue;
                };
                // The AUR's own out-of-date marker is the one thing here it
                // alone knows.
                model.aur_flagged_outdated = Set(Some(result.out_of_date.unwrap_or(0) != 0));
                model.aur_missing = Set(Some(false));
                // The version is the AUR's; the checkout is only read for the
                // metadata and the `git+` sources, so one that cannot be read
                // costs those and not the version check.
                let resolved = check.read(SourceinfoPurpose::Vcs, unreadable).await;
                let upstream = check
                    .record(&mut model, &result.version, resolved.as_ref(), &mut round)
                    .await?;
                // Only now refresh the snapshot -- a `git fetch` -- and only
                // when something looks new, rather than re-fetching every
                // package on every check.
                if upstream.is_outdated()
                    && let Err(e) = store.refresh(&package.source_data).await
                {
                    check.refresh_failed(RefreshTarget::Snapshot, &e);
                }
            }
            SourceData::Git { .. } => {
                // No cheap metadata API for arbitrary git remotes, so always
                // refresh: an incremental `git fetch` of the checkout.
                if let Err(e) = store.refresh(&package.source_data).await {
                    check.refresh_failed(RefreshTarget::Git, &e);
                    continue;
                }
                // A patch that no longer applies against a new upstream commit
                // costs this package its tracking this round, not the rest of
                // the check; its build will fail the same way later.
                let Some(resolved) = check.read(SourceinfoPurpose::Version, unreadable).await
                else {
                    continue;
                };
                // Only the version in the PKGBUILD; a ref moving without a
                // version bump is what the `git+` source check catches.
                let version = resolved.sourceinfo.base.version.to_string();
                check
                    .record(&mut model, &version, Some(&resolved), &mut round)
                    .await?;
            }
            // Updated only by a new upload, so nothing to check.
            SourceData::Upload { .. } => continue,
        }
        save_package(db, activity, model, &package.name).await;
    }

    // Detection is the only thing that knows a package went out of date —
    // including a VCS package whose upstream moved without a pkgver bump — so
    // with this on, the rebuild is queued here rather than waiting for the
    // auto-update schedule, which could be a day away.
    //
    // Reuses the auto-update job's own selection rather than restating it:
    // out-of-date packages whose last build succeeded. A package whose build
    // is failing stays flagged for a human instead of being retried in a loop.
    let build_now = settings::get(db, settings::key::BUILD_ON_NEW_VERSION, None).await?;
    if build_now.value
        && let Err(e) = package_update_all_outdated(services).await
    {
        activity.emit(Event::UpdateQueueFailed {
            error: format!("{e:#}"),
        });
    }

    Ok(())
}

/// One package's turn in a version check.
struct Check<'a> {
    services: &'a Services,
    package: &'a packages::Model,
}

impl Check<'_> {
    /// The package's `.SRCINFO` and the metadata beside it, from its
    /// checkout; `None` when it cannot be read, which is reported the first
    /// time in a row it happens.
    async fn read(
        &self,
        purpose: SourceinfoPurpose,
        unreadable: &mut Unreadable,
    ) -> Option<Resolved> {
        let package = self.package;
        match self
            .services
            .store
            .sourceinfo_and_metadata(&package.source_data, package.patch.as_deref())
            .await
        {
            Ok(resolved) => {
                unreadable.0.remove(&package.id);
                Some(resolved)
            }
            Err(e) if unreadable.0.insert(package.id) => {
                self.services.activity.emit(Event::SourceinfoFailed {
                    pkg: package.name.as_str().into(),
                    purpose,
                    error: format!("{e:#}"),
                });
                None
            }
            Err(e) => {
                tracing::debug!("{} still cannot be read: {e:#}", package.name);
                None
            }
        }
    }

    /// Record what the check found -- the upstream version, the metadata the
    /// checkout gave, whether the package is out of date -- and log a version
    /// that is news.
    async fn record(
        &self,
        model: &mut packages::ActiveModel,
        version: &str,
        resolved: Option<&Resolved>,
        round: &mut RoundCache,
    ) -> anyhow::Result<Upstream> {
        let Services { db, activity, .. } = self.services;
        let name = &self.package.name;
        // Not scoped to a platform: this is a question about the package
        // rather than about one architecture.
        let built = latest_successful_version_any_platform(db, self.package.id).await?;
        let mut vcs_moved = false;
        if let Some(resolved) = resolved {
            apply_source_metadata(model, &resolved.metadata);
            vcs_moved = self.vcs_moved(&resolved.sourceinfo, round).await;
        }
        // Only newer counts, never merely different: a VCS package's AUR
        // version is from when its PKGBUILD was last touched, which may be
        // older than what was built from the live source.
        let upstream = Upstream {
            newer: upstream_is_newer(version, built.as_deref(), name, activity),
            vcs_moved,
        };
        model.upstream_version = Set(Some(version.to_string()));
        model.out_of_date = Set(upstream.is_outdated());
        // The upstream version already reported, if the package was out of
        // date going in.
        let reported = self
            .package
            .out_of_date
            .then_some(self.package.upstream_version.as_deref())
            .flatten();
        report_detected(activity, name, reported, version, built, upstream);
        Ok(upstream)
    }

    /// Whether one of the package's `git+` sources moved since the last check.
    ///
    /// A sync that fails is logged and counts as not moved: a later round
    /// retries.
    async fn vcs_moved(
        &self,
        sourceinfo: &alpm_srcinfo::SourceInfoV1,
        round: &mut RoundCache,
    ) -> bool {
        match sync_vcs_sources(&self.services.db, self.package.id, sourceinfo, round).await {
            Ok(sync) => sync == VcsSync::Moved,
            Err(e) => {
                self.services.activity.emit(Event::VcsSyncFailed {
                    pkg: self.package.name.as_str().into(),
                    error: format!("{e:#}"),
                });
                false
            }
        }
    }

    fn refresh_failed(&self, target: RefreshTarget, error: &anyhow::Error) {
        self.services.activity.emit(Event::SourceRefreshFailed {
            pkg: self.package.name.as_str().into(),
            target,
            error: format!("{error:#}"),
        });
    }
}

/// What a version check found about one package's upstream.
#[derive(Clone, Copy, Debug)]
struct Upstream {
    /// Its version is newer than what was last built, or nothing was built.
    newer: bool,
    /// One of its `git+` sources moved since the last check.
    vcs_moved: bool,
}

impl Upstream {
    const fn is_outdated(self) -> bool {
        self.newer || self.vcs_moved
    }
}

/// Log a new upstream version, once: when the package goes out of date, or
/// when upstream moves again while it already is. The check runs every hour
/// and a package can stay out of date for many of them -- its auto-update off,
/// or its builds failing -- so "still newer" is not news.
///
/// New commits in a VCS source are news each time: the sync that reports them
/// only does so when the recorded commit moves.
fn report_detected(
    activity: &ActivityLog,
    pkgbase: &str,
    reported: Option<&str>,
    version: &str,
    built: Option<String>,
    upstream: Upstream,
) {
    if upstream.newer && reported != Some(version) {
        activity.emit(Event::VersionDetected {
            pkg: pkgbase.into(),
            version: version.to_string(),
            built,
            vcs: false,
        });
    } else if upstream.vcs_moved && !upstream.newer {
        activity.emit(Event::VersionDetected {
            pkg: pkgbase.into(),
            version: version.to_string(),
            built: None,
            vcs: true,
        });
    }
}

/// Whether `upstream` is a newer version than what was last built.
///
/// A package that has never been built counts as outdated. When the two
/// versions cannot be compared (either side is not valid alpm syntax) we fall
/// back to "did the string change", which errs towards scheduling a build:
/// reporting "not newer" would silently freeze the package forever, whereas a
/// spurious rebuild is merely wasted work.
fn upstream_is_newer(
    upstream: &str,
    built: Option<&str>,
    package: &str,
    activity: &ActivityLog,
) -> bool {
    let Some(built) = built else {
        return true;
    };
    match vercmp(upstream, built) {
        Some(ordering) => ordering == std::cmp::Ordering::Greater,
        None => {
            activity.emit(Event::VersionCompareFallback {
                pkg: package.into(),
                upstream_version: upstream.to_string(),
                built_version: built.to_string(),
            });
            upstream != built
        }
    }
}

/// Persist the version-check outcome for one package. A write failure only
/// costs this package one round of tracking, so it is logged, not propagated.
async fn save_package(
    db: &DatabaseConnection,
    activity: &ActivityLog,
    model: packages::ActiveModel,
    name: &str,
) {
    if let Err(e) = model.update(db).await {
        activity.emit(Event::VersionCheckStoreFailed {
            pkg: name.into(),
            error: format!("{e:#}"),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::aur_query_name;

    #[test]
    fn aur_queries_use_a_produced_name_even_with_only_one_child() {
        assert_eq!(
            aur_query_name("demo-base", Some(r#"["demo-cli"]"#)),
            "demo-cli"
        );
        assert_eq!(
            aur_query_name("demo-base", Some(r#"["demo-cli","demo-gui"]"#)),
            "demo-cli"
        );
        assert_eq!(aur_query_name("demo", None), "demo");
        assert_eq!(aur_query_name("demo", Some("[]")), "demo");
    }
}
