//! Recording the dependencies of packages from before AURCache tracked them.
//!
//! The upgrade from 0.5.0 queues this as an operation, because resolving
//! takes each package's source and the server's resolver, which a migration
//! has neither of. It runs at the next start, in the background, and shows
//! as an operation in progress while it does. The builds 0.5.0 left pending
//! wait for it: each is released as its package is resolved, or by the
//! pass that queues what is missing once the backfill is done.
//!
//! 0.5.0 also stored an AUR package under the name it was added by, which
//! for a split package is not its base, and the AUR has no source by that
//! name. The upgrade renames the ones whose files say what their base is;
//! [`canonicalize_names`] asks the AUR about the rest.

use crate::package::update::{announce_unblocked, resync_graph};
use crate::services::Services;
use aurcache_activitylog::events::Event;
use aurcache_db::helpers::operations::{self, KIND_DEPENDENCY_BACKFILL};
use aurcache_db::packages::{self, SourceData};
use aurcache_db::prelude::Packages;
use sea_orm::ActiveValue::Set;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, QueryOrder,
};
use serde::Serialize;
use std::time::Duration;
use tokio::sync::watch;

/// How long the server waits, as it starts, for the AUR to name the package
/// bases of 0.5.0's packages before it asks again in the background.
const CANONICALIZE_WAIT: Duration = Duration::from_secs(60);

/// How long the backfill waits for the official databases before it lets the
/// builds 0.5.0 left pending go without their dependencies recorded.
const OFFICIAL_WAIT: Duration = Duration::from_secs(10 * 60);

/// How one package's dependencies were recorded.
#[derive(Serialize)]
pub struct BackfillEntry {
    pub pkgbase: String,
    #[serde(flatten)]
    pub outcome: BackfillOutcome,
}

/// Whether a package's dependencies could be resolved.
#[derive(Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum BackfillOutcome {
    Resolved,
    /// Left for a resync of the package once whatever failed is fixed.
    Failed {
        error: String,
    },
}

/// Whether a backfill was waiting to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backfill {
    /// None was queued.
    NotQueued,
    /// It ran to the end; the failures are in its operation.
    Ran,
}

/// Queue whatever should be building and has no build, after the backfill an
/// upgrade from 0.5.0 queued when there is one -- until it has recorded what a
/// package from 0.5.0 depends on, every one reads as ready to build.
///
/// The backfill runs in the background, after naming 0.5.0's packages after
/// their bases: as the server starts for a while, then for as long as it
/// takes. The receiver turns `true` once there is no backfill to wait for,
/// which is what the version check waits on: until then it would ask the AUR
/// by names that are no package base and report those packages removed.
pub async fn start(services: &Services) -> watch::Receiver<bool> {
    let queued = queued(&services.db).await.unwrap_or_else(|e| {
        tracing::warn!("Could not tell whether a dependency backfill is queued: {e:#}");
        false
    });
    let (backfilled, done) = watch::channel(!queued);
    if !queued {
        enqueue_missing(services).await;
        return done;
    }

    let named = match tokio::time::timeout(CANONICALIZE_WAIT, canonicalize_names(services)).await {
        Ok(Ok(())) => true,
        Ok(Err(e)) => {
            warn_unnamed(&e);
            false
        }
        Err(_) => {
            tracing::warn!(
                "The AUR did not answer for the package bases of 0.5.0's packages in time"
            );
            false
        }
    };
    let services = services.clone();
    tokio::spawn(async move {
        if !named && let Err(e) = canonicalize_names(&services).await {
            warn_unnamed(&e);
        }
        let outcome = backfill_dependencies(&services).await;
        backfilled.send_replace(true);
        match outcome {
            Ok(Backfill::Ran) => enqueue_missing(&services).await,
            Ok(Backfill::NotQueued) => {}
            Err(e) => tracing::error!("Dependency backfill stopped: {e:#}"),
        }
    });
    done
}

fn warn_unnamed(e: &anyhow::Error) {
    tracing::warn!("Could not ask the AUR for the package bases of 0.5.0's packages: {e:#}");
}

/// Give a build to anything that should be building and has none: a package
/// added while the server was down, or whose build row was lost.
async fn enqueue_missing(services: &Services) {
    if let Err(e) =
        super::enqueue::enqueue_missing_buildable_packages(&services.db, &services.activity).await
    {
        services.activity.emit(Event::StartupEnqueueFailed {
            error: format!("{e:#}"),
        });
    }
}

/// Whether the upgrade from 0.5.0 queued a backfill that has not finished.
///
/// # Errors
///
/// A database error from finding the operation.
pub async fn queued(db: &DatabaseConnection) -> anyhow::Result<bool> {
    Ok(operations::pending(db, KIND_DEPENDENCY_BACKFILL)
        .await?
        .is_some())
}

/// Run the dependency backfill the upgrade from 0.5.0 queued, if one is
/// waiting.
///
/// Every package in turn, each recorded as it finishes. A restart part-way
/// through runs it again from the start, which only re-adds what is missing.
///
/// # Errors
///
/// A database error from finding the operation or the packages; a package
/// that fails to resolve is recorded and the rest carry on.
pub async fn backfill_dependencies(services: &Services) -> anyhow::Result<Backfill> {
    let db = &services.db;
    let Some(operation) = operations::pending(db, KIND_DEPENDENCY_BACKFILL).await? else {
        return Ok(Backfill::NotQueued);
    };
    // Before the official databases are read, every package with a dependency
    // fails to resolve -- and nothing would run this again. The server reads
    // them as it starts, so this waits seconds; through a mirror outage, as
    // long as the outage lasts, which only delays what a backfill records. The
    // builds held for it are not kept that long: they build as 0.5.0 would
    // have, and what they depend on is recorded once it can be.
    if tokio::time::timeout(OFFICIAL_WAIT, services.client.official.wait_until_read())
        .await
        .is_err()
    {
        tracing::warn!(
            "The official repository databases are still unread after {} minutes; the builds \
             0.5.0 left pending go ahead without their dependencies recorded",
            OFFICIAL_WAIT.as_secs() / 60
        );
        for package in Packages::find().all(db).await? {
            release(services, package.id).await;
        }
        services.client.official.wait_until_read().await;
    }
    tracing::info!("Recording the dependencies of packages from before they were tracked");
    let packages = Packages::find()
        .order_by_asc(packages::Column::Id)
        .all(db)
        .await?;

    let (mut completed, mut failed) = (0, 0);
    for package in packages {
        let outcome = match resync_graph(services, &package).await {
            Ok(()) => {
                completed += 1;
                BackfillOutcome::Resolved
            }
            Err(e) => {
                failed += 1;
                tracing::warn!(
                    "could not record the dependencies of {}: {e:#}",
                    package.name
                );
                // Nothing recorded to wait for: its held builds go ahead, as
                // 0.5.0 would have built them.
                release(services, package.id).await;
                BackfillOutcome::Failed {
                    error: format!("{e:#}"),
                }
            }
        };
        let entry = BackfillEntry {
            pkgbase: package.name,
            outcome,
        };
        operations::append(db, operation.id, completed, failed, &[entry], false).await?;
    }
    operations::append::<_, BackfillEntry>(db, operation.id, completed, failed, &[], true).await?;
    Ok(Backfill::Ran)
}

/// Let a package's held builds go ahead as far as its recorded dependencies
/// allow -- with none recorded, all of them. Logged rather than returned: one
/// that stays held is retried by the pass after the backfill.
async fn release(services: &Services, pkg_id: i32) {
    let released = async {
        let txn = aurcache_db::helpers::txn::begin_write(&services.db).await?;
        let unblocked = crate::worker_complete::resync_pending_builds(&txn, pkg_id).await?;
        txn.commit().await?;
        anyhow::Ok(unblocked)
    }
    .await;
    match released {
        Ok(unblocked) => announce_unblocked(services, &unblocked).await,
        Err(e) => tracing::warn!("could not release the held builds of package #{pkg_id}: {e:#}"),
    }
}

/// Rename every AUR package stored under one of a split package's names to
/// its package base, which is what the AUR has a source for.
///
/// For the ones an upgrade from 0.5.0 could not rename: it reads the base
/// from a package's files, and one never built has none. Asked of the AUR in
/// one request, so it runs as the server starts, ahead of the version check
/// that would find no such base and call the package removed. A package whose
/// base is already a package here is left alone and named in the log: they
/// are one package, and which to keep is the operator's call.
///
/// # Errors
///
/// The AUR could not be asked, or a rename could not be written.
pub async fn canonicalize_names(services: &Services) -> anyhow::Result<()> {
    let db = &services.db;
    let packages: Vec<packages::Model> = Packages::find()
        .all(db)
        .await?
        .into_iter()
        .filter(|p| matches!(p.source_data, SourceData::Aur { .. }))
        .collect();
    let names: Vec<&str> = packages.iter().map(|p| p.name.as_str()).collect();
    if names.is_empty() {
        return Ok(());
    }
    let bases = services.client.resolve_bases(&names).await?;
    for package in &packages {
        let Some(base) = bases
            .get(&package.name)
            .filter(|base| **base != package.name)
        else {
            continue;
        };
        if Packages::find()
            .filter(packages::Column::Name.eq(base.as_str()))
            .one(db)
            .await?
            .is_some()
        {
            tracing::warn!(
                "{} is one of the split packages of {base}, which is also a package here; remove \
                 {} -- its builds come from {base}'s",
                package.name,
                package.name
            );
            continue;
        }
        packages::ActiveModel {
            id: Set(package.id),
            name: Set(base.clone()),
            source_data: Set(SourceData::Aur { name: base.clone() }),
            ..Default::default()
        }
        .update(db)
        .await?;
        // Logs are kept by package base; what the old name logged follows it.
        let (from, to) = (
            aurcache_common::fs::build_log_dir(&package.name),
            aurcache_common::fs::build_log_dir(base),
        );
        if from.exists()
            && !to.exists()
            && let Err(e) = tokio::fs::rename(&from, &to).await
        {
            tracing::warn!("could not move {} to {}: {e}", from.display(), to.display());
        }
        tracing::info!("{}: renamed to its package base, {base}", package.name);
    }
    Ok(())
}
