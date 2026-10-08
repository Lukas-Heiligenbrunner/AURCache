use crate::repository::Repository;
use crate::snapshot::SnapshotStore;
use anyhow::bail;
use aurcache_activitylog::events::Event;
use aurcache_db::packages::SourceData;
use aurcache_db::prelude::{Builds, Dependencies, Files, PackageVcsSources, Packages, Settings};
use aurcache_db::{builds, dependencies, files, package_vcs_sources, packages, settings};
use sea_orm::{
    ColumnTrait, Condition, ConnectionTrait, DatabaseConnection, EntityTrait, QueryFilter,
    QuerySelect,
};
use tracing::warn;

/// Delete packages outright: their rows, their artifacts' `repo.db` entries,
/// their build logs and their source checkouts -- all of it, or none of it.
/// The artifact files themselves are retired, and deleted by the repository
/// sweep once clients with an older `repo.db` have had time to fetch them.
///
/// **Refused while anything outside `pkg_ids` depends on one of them.** A
/// package something still needs keeps everything; deciding that it is no
/// longer needed is [`live_check`](crate::package::live_check::live_check)'s
/// job, and this is the last line enforcing its answer. Several at once so a
/// chain or a cycle that is being removed together can be: each member is
/// depended on, but only from inside the set.
///
/// Ids that no longer exist are skipped, so a concurrent removal of the same
/// package is not an error.
pub async fn package_delete(
    db: &DatabaseConnection,
    store: &SnapshotStore,
    repo: &Repository,
    pkg_ids: &[i32],
) -> anyhow::Result<()> {
    if pkg_ids.is_empty() {
        return Ok(());
    }

    let mut update = repo.begin(db).await?;
    let doomed: Vec<packages::Model> = Packages::find()
        .filter(packages::Column::Id.is_in(pkg_ids.iter().copied()))
        .all(db)
        .await?;
    let ids: Vec<i32> = doomed.iter().map(|pkg| pkg.id).collect();
    // Checked here as well as in the transaction: a refusal is not worth the
    // retries a failed commit gets.
    refuse_if_needed(db, &doomed, &ids).await?;
    for file in update.published_files_of(db, &ids).await? {
        update.retire(&file)?;
    }
    let doomed = update
        .commit(db, |txn| {
            let doomed = doomed.clone();
            Box::pin(async move { delete_rows(txn, &doomed).await })
        })
        .await?;

    // Stored outside the database and not part of the repository, so after the
    // rows are gone: a stray directory is only wasted bytes.
    //
    // A checkout directory is not always one package's alone, so the remaining
    // packages' sources say which ones must stay. Without them the removal is
    // skipped rather than guessed at; the boot-time prune catches it later.
    // Only the one column this needs: the full rows carry build logs'
    // metadata and JSON blobs the checkout comparison never looks at.
    let remaining: Option<Vec<SourceData>> = match Packages::find()
        .select_only()
        .column(packages::Column::SourceData)
        .into_tuple::<SourceData>()
        .all(db)
        .await
    {
        Ok(rows) => Some(rows),
        Err(e) => {
            warn!("could not list packages, leaving source checkouts in place: {e}");
            None
        }
    };
    for pkg in &doomed {
        crate::build_logger::remove_package_logs(&pkg.name).await;
        if let Some(remaining) = &remaining
            && let Err(e) = store.remove_checkout(&pkg.source_data, remaining).await
        {
            repo.log().emit(Event::CheckoutRemoveFailed {
                pkg: pkg.name.as_str().into(),
                error: format!("{e:#}"),
            });
        }
    }
    Ok(())
}

/// Fail if a package outside `ids` depends on one inside it.
async fn refuse_if_needed<C: ConnectionTrait>(
    db: &C,
    doomed: &[packages::Model],
    ids: &[i32],
) -> anyhow::Result<()> {
    let still_needed: Vec<i32> = Dependencies::find()
        .filter(dependencies::Column::DependeeId.is_in(ids.iter().copied()))
        .filter(dependencies::Column::DependentId.is_not_in(ids.iter().copied()))
        .select_only()
        .column(dependencies::Column::DependeeId)
        .into_tuple()
        .all(db)
        .await?;
    if !still_needed.is_empty() {
        let names: Vec<&str> = doomed
            .iter()
            .filter(|pkg| still_needed.contains(&pkg.id))
            .map(|pkg| pkg.name.as_str())
            .collect();
        bail!(
            "not deleting {}: other packages still depend on it",
            names.join(", ")
        );
    }
    Ok(())
}

/// The rows, in one transaction, children before the package rows.
///
/// `files.package_id` cascades, and nothing would notice the cascade taking
/// those rows -- but every child is deleted explicitly anyway, so nothing here
/// depends on the connection having `foreign_keys` on.
async fn delete_rows(
    txn: &sea_orm::DatabaseTransaction,
    doomed: &[packages::Model],
) -> anyhow::Result<Vec<packages::Model>> {
    // Locked before the dependents are counted: on Postgres a new dependency
    // row referencing one of these then waits for this transaction and fails on
    // the missing package, instead of being inserted after the check and
    // silently cascaded away with it. (SQLite serialises writers anyway.)
    let ids: Vec<i32> = Packages::find()
        .filter(packages::Column::Id.is_in(doomed.iter().map(|pkg| pkg.id)))
        .select_only()
        .column(packages::Column::Id)
        .lock_exclusive()
        .into_tuple()
        .all(txn)
        .await?;

    refuse_if_needed(txn, doomed, &ids).await?;

    Files::delete_many()
        .filter(files::Column::PackageId.is_in(ids.iter().copied()))
        .exec(txn)
        .await?;
    Builds::delete_many()
        .filter(builds::Column::PkgId.is_in(ids.iter().copied()))
        .exec(txn)
        .await?;
    // Both ends: what these packages needed, and the links among themselves.
    // Nothing outside the set points at them, as checked above.
    Dependencies::delete_many()
        .filter(
            Condition::any()
                .add(dependencies::Column::DependentId.is_in(ids.iter().copied()))
                .add(dependencies::Column::DependeeId.is_in(ids.iter().copied())),
        )
        .exec(txn)
        .await?;
    Settings::delete_many()
        .filter(settings::Column::PkgId.is_in(ids.iter().copied()))
        .exec(txn)
        .await?;
    PackageVcsSources::delete_many()
        .filter(package_vcs_sources::Column::PackageId.is_in(ids.iter().copied()))
        .exec(txn)
        .await?;
    Packages::delete_many()
        .filter(packages::Column::Id.is_in(ids.iter().copied()))
        .exec(txn)
        .await?;

    Ok(doomed
        .iter()
        .filter(|pkg| ids.contains(&pkg.id))
        .cloned()
        .collect())
}
