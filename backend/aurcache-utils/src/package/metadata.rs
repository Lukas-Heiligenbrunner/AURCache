//! Mirroring package metadata onto the package row.
//!
//! `/api/package/<pkgbase>` used to fetch this from the AUR on every request,
//! which made the route ~128ms where the rest of it is ~2ms and spent one of
//! the AUR's 4000 daily calls per page view. Persisting it turns that into a
//! plain column read.
//!
//! It is read from the package's own source checkout, not the AUR. The
//! snapshot store already clones every package's repository to resolve its
//! sources, so the data is on disk either way, and reading it there fixes two
//! things the RPC could not:
//!
//! - **Git-sourced packages get metadata at all.** They have no AUR entry, so
//!   their page showed no description, licenses or maintainer.
//! - **It reflects the patched PKGBUILD**, which is what actually gets built.
//!
//! Written from two places, which between them cover a package's whole life:
//! [`refresh_source_metadata`] when it is added, and the version-check
//! scheduler on its interval.

use crate::package::source_metadata::SourceMetadata;
use crate::snapshot::SnapshotStore;
use aurcache_activitylog::activity_utils::ActivityLog;
use aurcache_activitylog::events::Event;
use aurcache_db::packages;
use aurcache_db::prelude::Packages;
use sea_orm::ActiveValue::Set;
use sea_orm::{ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};

/// Copy metadata read from a checkout onto a row.
///
/// `upstream_version` is deliberately not set here: whether a version change
/// also means the package is out of date is a different question, and the
/// caller answers it.
pub fn apply_source_metadata(model: &mut packages::ActiveModel, metadata: &SourceMetadata) {
    model.source_description = Set(metadata.description.clone());
    model.source_maintainer = Set(metadata.maintainer.clone());
    model.source_project_url = Set(metadata.project_url.clone());
    model.source_licenses = Set(metadata.licenses.clone());
    model.source_first_submitted = Set(metadata.first_submitted);
    model.source_last_modified = Set(metadata.last_modified);
}

/// Read and store metadata for freshly added packages.
///
/// Called at add time so a package is never without it: the API reads straight
/// from the row and has no live fallback, and waiting for the next scheduled
/// version check would leave a new package with no description or maintainer
/// for up to an hour.
///
/// Best-effort per package: one whose source cannot be read should render
/// without a description rather than fail the add.
pub async fn refresh_source_metadata(
    store: &SnapshotStore,
    db: &DatabaseConnection,
    activity: &ActivityLog,
    pkgbases: &[String],
) {
    let failed = |pkgbase: &str, error: String| {
        activity.emit(Event::SourceMetadataFailed {
            pkg: pkgbase.into(),
            error,
        });
    };
    for pkgbase in pkgbases {
        let row = match Packages::find()
            .filter(packages::Column::Name.eq(pkgbase))
            .one(db)
            .await
        {
            Ok(Some(row)) => row,
            Ok(None) => continue,
            Err(e) => {
                tracing::warn!("could not load {pkgbase} for metadata refresh: {e}");
                continue;
            }
        };

        let metadata = match store
            .source_metadata(&row.source_data, row.patch.as_deref())
            .await
        {
            Ok(metadata) => metadata,
            Err(e) => {
                failed(pkgbase, format!("{e:#}"));
                continue;
            }
        };

        let mut model = packages::ActiveModel {
            id: Set(row.id),
            ..Default::default()
        };
        apply_source_metadata(&mut model, &metadata);
        if let Err(e) = model.update(db).await {
            failed(pkgbase, e.to_string());
        }
    }
}
