//! Recording the dependencies of packages from before AURCache tracked them.
//!
//! The upgrade from 0.5.0 queues this as an operation, because resolving
//! takes each package's source and the server's resolver, which a migration
//! has neither of. It runs at the next start, in the background, and shows
//! as an operation in progress while it does.

use crate::package::update::resync_graph;
use crate::services::Services;
use aurcache_db::helpers::operations::{self, KIND_DEPENDENCY_BACKFILL};
use aurcache_db::packages;
use aurcache_db::prelude::Packages;
use sea_orm::{EntityTrait, QueryOrder};
use serde::Serialize;

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
