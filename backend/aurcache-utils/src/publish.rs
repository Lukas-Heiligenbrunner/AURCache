//! Putting a finished build in the repository.
//!
//! A worker's part ends when the server accepts its completion: the build moves
//! to `PUBLISHING` and the worker moves on (see
//! [`accept_for_publishing`](crate::worker_complete::accept_for_publishing)).
//! Everything after that is the server's -- reading the packages, updating
//! the databases, recording the result -- and nothing the worker could do again
//! would help with any of it. Invalid artifacts fail the build; recoverable
//! finalization and dependent-promotion failures are retried by the server.

use crate::build_logger::append_build_output;
use crate::repository::{PublishedFile, Repository, Update};
use anyhow::{anyhow, bail};
use aurcache_activitylog::activity_utils::ActivityLog;
use aurcache_activitylog::events::Event;
use aurcache_common::api::log::BuildRef;
use aurcache_common::build_state::BuildState;
use aurcache_db::helpers::builds::refresh_package_status;
use aurcache_db::helpers::time::now_secs;
use aurcache_db::prelude::{Builds, Dependencies, Files, Packages};
use aurcache_db::{builds, dependencies, files, packages};
use pacman_mirrors::platforms::Platform;
use pacman_repo_utils::PackageEntry;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, PaginatorTrait, QueryFilter,
    QuerySelect, Set, TransactionTrait,
};
use std::path::PathBuf;
use std::time::Duration;
use tracing::{error, info, warn};

/// Whether publication has settled or still has recoverable work to finish.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PublicationOutcome {
    Finished,
    Retry,
}

/// Publish in the background until filesystem finalization and dependent
/// promotion finish. Keep one task per build through recoverable failures;
/// restarting the server resumes it from the same durable staging directory.
pub async fn publish_with_recovery(
    db: &DatabaseConnection,
    repo: &Repository,
    activity: &ActivityLog,
    build_id: i32,
) {
    loop {
        // Recover before describing the uploaded archives again: a blocked
        // rename should not repeatedly read a potentially huge package.
        match repo.recover(db).await {
            Ok(()) => {
                if publish_build(db, repo, activity, build_id).await == PublicationOutcome::Finished
                {
                    return;
                }
            }
            Err(error) => warn!("publication of build #{build_id} needs recovery: {error:#}"),
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

/// Attempt to publish a build from staging, or resume its dependent promotion.
///
/// Invalid artifacts fail the build. Recoverable errors retain staging and
/// return `Retry`; production callers use [`publish_with_recovery`].
pub async fn publish_build(
    db: &DatabaseConnection,
    repo: &Repository,
    activity: &ActivityLog,
    build_id: i32,
) -> PublicationOutcome {
    let build = match Builds::find_by_id(build_id).one(db).await {
        Ok(Some(build)) => build,
        Ok(None) => {
            let _ = tokio::fs::remove_dir_all(repo.staging_dir(build_id)).await;
            return PublicationOutcome::Finished;
        }
        Err(e) => {
            error!("could not load build #{build_id} for publication: {e}");
            return PublicationOutcome::Retry;
        }
    };
    if !matches!(
        build.status,
        BuildState::Publishing | BuildState::Successful
    ) {
        return PublicationOutcome::Finished;
    }
    if build.status == BuildState::Successful && !repo.staging_dir(build_id).is_dir() {
        return PublicationOutcome::Finished;
    }
    let pkgbase = Packages::find_by_id(build.pkg_id)
        .one(db)
        .await
        .ok()
        .flatten()
        .map(|pkg| pkg.name);

    let publication = if build.status == BuildState::Successful {
        Ok(Publication::Earlier)
    } else if recorded(&build) {
        // An earlier attempt committed the files and the repository; only
        // the status did not follow. Reading every archive again would redo
        // all of that for nothing.
        settle(db, &build)
            .await
            .map(|()| Publication::Now { packages: None })
    } else {
        publish(db, repo, &build)
            .await
            .map(|packages| Publication::Now {
                packages: Some(packages),
            })
    };
    match publication {
        Ok(publication) => {
            let published = pkgbase.as_ref().map(|pkgbase| BuildRef {
                pkgbase: pkgbase.clone(),
                number: build.number,
            });
            // Announced once, by the call that published it: a retry to
            // promote dependents, or a resume after a restart, has nothing new
            // to say.
            if let Publication::Now { packages } = publication {
                match packages {
                    Some(packages) => info!("published build #{build_id}: {packages} package(s)"),
                    None => info!("published build #{build_id}"),
                }
                log(
                    Some(activity),
                    pkgbase.as_deref(),
                    build.number,
                    "Published to the repository\n",
                )
                .await;
                if let Some(published) = &published {
                    activity.emit(Event::BuildPublished {
                        build: published.clone(),
                        version: Some(build.version.clone()).filter(|v| !v.is_empty()),
                    });
                }
            }
            match crate::worker_complete::trigger_dependents(db, build.pkg_id, build.platform).await
            {
                Ok(unblocked) => {
                    for unblocked in unblocked {
                        activity.emit(Event::BuildUnblocked {
                            build: unblocked,
                            by: published.clone(),
                        });
                    }
                }
                Err(e) => {
                    error!(
                        "Failed to trigger dependents of package {}: {e}",
                        build.pkg_id
                    );
                    if let Some(pkgbase) = &pkgbase {
                        activity.emit(Event::DependentsTriggerFailed {
                            pkg: pkgbase.as_str().into(),
                            error: e.to_string(),
                        });
                    }
                    return PublicationOutcome::Retry;
                }
            }
        }
        Err(e)
            if e.downcast_ref::<crate::repository::FinalizationPending>()
                .is_some() =>
        {
            warn!("publication of build #{build_id} needs recovery: {e:#}");
            return PublicationOutcome::Retry;
        }
        Err(e) => {
            warn!("publishing build #{build_id} failed: {e:#}");
            // The build worked and the package exists; it is the last step that
            // did not. That is worth an entry someone will come across, rather
            // than only a line in this process's journal.
            activity.emit(Event::PublishFailed {
                build: BuildRef {
                    pkgbase: pkgbase.clone().unwrap_or_else(|| "?".to_string()),
                    number: build.number,
                },
                error: format!("{e:#}"),
            });
            log(
                Some(activity),
                pkgbase.as_deref(),
                build.number,
                &format!("Publishing failed: {e:#}\n"),
            )
            .await;
            if let Err(e) = fail(db, &build).await {
                error!("could not mark build #{build_id} failed: {e}");
                if let Some(pkgbase) = &pkgbase {
                    activity.emit(Event::BuildMarkFailed {
                        build: BuildRef {
                            pkgbase: pkgbase.clone(),
                            number: build.number,
                        },
                        error: e.to_string(),
                    });
                }
                return PublicationOutcome::Retry;
            }
        }
    }
    let _ = tokio::fs::remove_dir_all(repo.staging_dir(build_id)).await;
    PublicationOutcome::Finished
}

/// Append to a build's log; a line that does not land is recorded where
/// someone will come across it, when there is somewhere to record it.
async fn log(activity: Option<&ActivityLog>, pkgbase: Option<&str>, number: i32, text: &str) {
    if let Some(pkgbase) = pkgbase
        && let Err(e) = append_build_output(pkgbase, number, text).await
    {
        match activity {
            Some(activity) => activity.emit(Event::BuildLogAppendFailed {
                build: BuildRef {
                    pkgbase: pkgbase.to_string(),
                    number,
                },
                error: e.to_string(),
            }),
            None => warn!("could not write to the log of {pkgbase}/{number}: {e}"),
        }
    }
}

/// What a call to [`publish_build`] did about the build's place in the
/// repository.
enum Publication {
    /// Published by this call; `packages` when it also knows how many.
    Now { packages: Option<usize> },
    /// Published by an earlier call, which leaves promoting its dependents.
    Earlier,
}

/// A staged package, read and named.
struct Staged {
    path: PathBuf,
    filename: String,
    entry: PackageEntry,
    size: i64,
}

/// A staged file to publish, before it is read.
struct Upload {
    path: PathBuf,
    filename: String,
}

/// One published file, as its `files` row will hold it.
#[derive(Clone)]
struct FileRow {
    filename: String,
    /// The `files` row it already has, if any.
    existing: Option<i32>,
    size: i64,
}

/// What the database is told, decided under the repository lock.
#[derive(Clone)]
struct Plan {
    rows: Vec<FileRow>,
    /// This package's files on this platform that the build no longer
    /// produces.
    stale: Vec<PublishedFile>,
    version: String,
    total_size: i64,
}

async fn publish(
    db: &DatabaseConnection,
    repo: &Repository,
    build: &builds::Model,
) -> anyhow::Result<usize> {
    let pkg = Packages::find_by_id(build.pkg_id)
        .one(db)
        .await?
        .ok_or_else(|| anyhow!("the package is gone"))?;
    let platform = build.platform;
    let staged = read_staging(repo, build, &pkg).await?;
    let packages = staged.len();

    // The slow part, and private: every archive read in full, before anything
    // is locked or changed.
    let mut described = Vec::with_capacity(staged.len());
    for Upload { path, filename } in staged {
        let size = i64::try_from(tokio::fs::metadata(&path).await?.len()).unwrap_or(i64::MAX);
        let entry = repo
            .describe(path.clone())
            .await
            .map_err(|e| e.context(format!("reading {filename}")))?;
        described.push(Staged {
            path,
            filename,
            entry,
            size,
        });
    }
    let version = parse_arch_pkg(&described[0].filename)?.version;

    let mut update = repo.begin(db).await?;
    let plan = plan(&update, db, &pkg, platform, &described, version).await?;
    for file in &plan.stale {
        update.retire(file)?;
    }
    for staged in described {
        update.add(platform, staged.path, staged.entry);
    }
    update
        .commit(db, |txn| {
            let pkg = pkg.clone();
            let plan = plan.clone();
            let build_id = build.id;
            Box::pin(async move { record(txn, build_id, &pkg, platform, &plan).await })
        })
        .await?;

    settle(db, build).await?;
    Ok(packages)
}

/// Whether the build's files and repository changes are already committed:
/// [`record`] sets its size in that transaction, and nothing else does.
fn recorded(build: &builds::Model) -> bool {
    build.size.is_some()
}

/// `PUBLISHING` -> `SUCCESSFUL`, once the build is in the repository.
///
/// Its own transaction, after the repository's: a build is successful when
/// its files can be downloaded, which is only once the update is finalized.
/// A failure here leaves the build `PUBLISHING` but [`recorded`], and the
/// retry comes straight back here.
async fn settle(db: &DatabaseConnection, build: &builds::Model) -> anyhow::Result<()> {
    let finished = async {
        let txn = db.begin().await?;
        Builds::update_many()
            .col_expr(builds::Column::Status, BuildState::Successful.into())
            .filter(builds::Column::Id.eq(build.id))
            .filter(builds::Column::Status.eq(BuildState::Publishing))
            .exec(&txn)
            .await?;
        refresh_package_status(&txn, build.pkg_id).await?;
        txn.commit().await?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    finished.map_err(|error| anyhow::Error::new(crate::repository::FinalizationPending(error)))
}

/// The staged files that are to be published, by filename.
///
/// Detached signatures are left out -- the repository moves them with their
/// package -- and so are makepkg's `-debug` packages (see
/// [`is_debug_artifact`]). What remains must be named for this package.
async fn read_staging(
    repo: &Repository,
    build: &builds::Model,
    pkg: &packages::Model,
) -> anyhow::Result<Vec<Upload>> {
    let expected = expected_pkgnames(pkg);
    let mut names = Vec::new();
    let mut entries = match tokio::fs::read_dir(repo.staging_dir(build.id)).await {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            bail!("no artifacts were uploaded")
        }
        Err(e) => return Err(e.into()),
    };
    while let Some(entry) = entries.next_entry().await? {
        if !entry.file_type().await?.is_file() {
            continue;
        }
        let filename = entry.file_name().to_string_lossy().into_owned();
        if filename.starts_with('.')
            || filename.ends_with(".sig")
            || filename.ends_with(".publishing")
        {
            continue;
        }
        if is_debug_artifact(&expected, &filename) {
            log(
                None,
                Some(&pkg.name),
                build.number,
                &format!("skipping debug package (not published): {filename}\n"),
            )
            .await;
            continue;
        }
        names.push(Upload {
            path: entry.path(),
            filename,
        });
    }
    if names.is_empty() {
        bail!("no publishable artifacts were uploaded");
    }
    let filenames: Vec<String> = names.iter().map(|upload| upload.filename.clone()).collect();
    validate_artifact_names(&expected, &filenames)?;
    names.sort_by(|a, b| a.filename.cmp(&b.filename));
    Ok(names)
}

/// Decide who owns each file and what goes stale, from what `update` reads.
///
/// A filename another package already publishes is refused, unless that
/// package depends on this one (the file moved between them) or no longer
/// exists (the row is a leftover, not a claim).
async fn plan(
    update: &Update<'_>,
    db: &DatabaseConnection,
    pkg: &packages::Model,
    platform: Platform,
    described: &[Staged],
    version: String,
) -> anyhow::Result<Plan> {
    // What this platform already publishes, in one query rather than one per
    // staged file (see `Update::published_files`).
    let known = update
        .published_files(
            db,
            platform,
            &described
                .iter()
                .map(|staged| staged.filename.clone())
                .collect::<Vec<_>>(),
        )
        .await?;
    let mut rows = Vec::with_capacity(described.len());
    for staged in described {
        let existing = known.get(&staged.filename);
        if let Some(existing) = existing
            && existing.package_id() != pkg.id
        {
            // Counted rather than read: all this needs is whether the row is
            // there, and loading the model would also have to deserialize
            // columns like `source_data`.
            let owner_exists = Packages::find_by_id(existing.package_id())
                .count(db)
                .await?
                > 0;
            let owner_depends_on_us = Dependencies::find()
                .filter(dependencies::Column::DependentId.eq(existing.package_id()))
                .filter(dependencies::Column::DependeeId.eq(pkg.id))
                .count(db)
                .await?
                > 0;
            if owner_exists && !owner_depends_on_us {
                bail!(
                    "File '{}' is already produced by another package",
                    staged.filename
                );
            }
        }
        rows.push(FileRow {
            filename: staged.filename.clone(),
            existing: existing.map(PublishedFile::id),
            size: staged.size,
        });
    }

    let published: Vec<&str> = described.iter().map(|s| s.filename.as_str()).collect();
    let stale = update
        .published_files_of(db, &[pkg.id])
        .await?
        .into_iter()
        .filter(|file| file.platform() == platform && !published.contains(&file.filename()))
        .collect();

    Ok(Plan {
        rows,
        stale,
        version,
        total_size: described.iter().map(|s| s.size).sum(),
    })
}

/// The one database transaction: the files, the build's result and what it
/// means for the package, together.
///
/// Conditioned on the build still being `PUBLISHING`, so a build cancelled or
/// deleted meanwhile is never recorded as published.
async fn record(
    txn: &sea_orm::DatabaseTransaction,
    build_id: i32,
    pkg: &packages::Model,
    platform: Platform,
    plan: &Plan,
) -> anyhow::Result<()> {
    let marked = Builds::update_many()
        .col_expr(builds::Column::Version, plan.version.clone().into())
        .col_expr(builds::Column::Size, Some(plan.total_size).into())
        .col_expr(builds::Column::EndTime, Some(now_secs()).into())
        .filter(builds::Column::Id.eq(build_id))
        .filter(builds::Column::Status.eq(BuildState::Publishing))
        .exec(txn)
        .await?;
    if marked.rows_affected == 0 {
        bail!("build #{build_id} is no longer being published");
    }

    // New rows go in one statement. Updates stay one per row: each touches a
    // distinct id, and a rebuild at the same version is updated even when
    // otherwise unchanged — same filename, different contents.
    let new_rows: Vec<files::ActiveModel> = plan
        .rows
        .iter()
        .filter(|row| row.existing.is_none())
        .map(|row| files::ActiveModel {
            filename: Set(row.filename.clone()),
            platform: Set(platform),
            package_id: Set(pkg.id),
            size: Set(Some(row.size)),
            ..Default::default()
        })
        .collect();
    if !new_rows.is_empty() {
        files::Entity::insert_many(new_rows).exec(txn).await?;
    }
    for row in &plan.rows {
        if let Some(id) = row.existing {
            files::ActiveModel {
                id: Set(id),
                package_id: Set(pkg.id),
                size: Set(Some(row.size)),
                ..Default::default()
            }
            .update(txn)
            .await?;
        }
    }
    if !plan.stale.is_empty() {
        Files::delete_many()
            .filter(files::Column::Id.is_in(plan.stale.iter().map(PublishedFile::id)))
            .exec(txn)
            .await?;
    }

    Packages::update_many()
        .col_expr(packages::Column::OutOfDate, false.into())
        .col_expr(
            packages::Column::UpstreamVersion,
            Some(plan.version.clone()).into(),
        )
        .filter(packages::Column::Id.eq(pkg.id))
        .exec(txn)
        .await?;
    refresh_package_status(txn, pkg.id).await?;
    Ok(())
}

/// Mark a build that could not be published as failed; its package's status
/// follows.
async fn fail(db: &DatabaseConnection, build: &builds::Model) -> anyhow::Result<()> {
    let txn = db.begin().await?;
    let marked = Builds::update_many()
        .col_expr(builds::Column::Status, BuildState::Failed.into())
        .col_expr(builds::Column::EndTime, Some(now_secs()).into())
        .filter(builds::Column::Id.eq(build.id))
        .filter(builds::Column::Status.eq(BuildState::Publishing))
        .exec(&txn)
        .await?;
    if marked.rows_affected > 0 {
        refresh_package_status(&txn, build.pkg_id).await?;
    }
    txn.commit().await?;
    Ok(())
}

/// The builds a restart interrupted mid-publish.
///
/// Nothing public happened to them yet -- the database transaction is what
/// moves a build out of `PUBLISHING` -- so publishing them again from their
/// staging directories is safe.
pub async fn interrupted(db: &DatabaseConnection, repo: &Repository) -> anyhow::Result<Vec<i32>> {
    let mut interrupted: Vec<i32> = Builds::find()
        .select_only()
        .column(builds::Column::Id)
        .filter(builds::Column::Status.eq(BuildState::Publishing))
        .into_tuple()
        .all(db)
        .await?;
    // Successful builds with staging still present may have crashed before
    // promoting their dependents. Replaying that step is idempotent.
    let mut staging = match tokio::fs::read_dir(repo.staging_root()).await {
        Ok(staging) => staging,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(interrupted),
        Err(error) => return Err(error.into()),
    };
    while let Some(entry) = staging.next_entry().await? {
        let Ok(id) = entry.file_name().to_string_lossy().parse::<i32>() else {
            continue;
        };
        if let Some(build) = Builds::find_by_id(id).one(db).await?
            && build.status == BuildState::Successful
        {
            interrupted.push(id);
        }
    }
    interrupted.sort_unstable();
    interrupted.dedup();
    Ok(interrupted)
}

/// The package names a build of `pkg` may produce: its pkgbase plus any
/// split-package names recorded on the package row.
pub fn expected_pkgnames(pkg: &packages::Model) -> Vec<String> {
    let mut names = vec![pkg.name.clone()];
    for name in aurcache_db::lists::json_list(pkg.split_packages.as_deref()) {
        if !names.contains(&name) {
            names.push(name);
        }
    }
    names
}

#[derive(Debug, Clone)]
struct ParsedPkg {
    name: String,
    version: String,
}

/// Parse an Arch package filename into its name and version.
///
/// e.g. `hello-2.12.1-1-x86_64.pkg.tar.zst` -> name `hello`, version `2.12.1-1`.
fn parse_arch_pkg(filename: &str) -> anyhow::Result<ParsedPkg> {
    let (base, _) = filename
        .rsplit_once(".pkg.tar.")
        .ok_or_else(|| anyhow!("Invalid pkg filename: {filename}"))?;

    let parts: Vec<&str> = base.split('-').collect();
    if parts.len() < 4 {
        bail!("Invalid pkg filename format: {filename}");
    }

    let pkgrel = parts[parts.len() - 2];
    let pkgver = parts[parts.len() - 3];
    let name = parts[..parts.len() - 3].join("-");

    Ok(ParsedPkg {
        name,
        version: format!("{pkgver}-{pkgrel}"),
    })
}

/// Sanity-check uploaded artifact filenames against the set of package names the
/// server expects for this package (its pkgbase plus any split packages).
///
/// This blocks *wrong-named* artifacts (e.g. a worker uploading `openssh-*`
/// under a job for `hello`); it does **not** and cannot verify the *contents* of
/// a correctly-named package (see design non-goals). Signature sidecars
/// (`*.sig`) and hidden helper files are ignored; every remaining file must be a
/// `*.pkg.tar.*` whose parsed pkgname is in `expected`.
///
/// Debug packages are dropped before this runs — see [`is_debug_artifact`].
fn validate_artifact_names(expected: &[String], filenames: &[String]) -> anyhow::Result<()> {
    if expected.is_empty() {
        bail!("no expected package names to validate against");
    }
    for filename in filenames {
        if filename.starts_with('.')
            || filename.ends_with(".sig")
            || filename.ends_with(".publishing")
        {
            continue;
        }
        if !filename.contains(".pkg.tar") {
            bail!("unexpected non-package artifact: {filename}");
        }
        let parsed = parse_arch_pkg(filename)?;
        if !expected.iter().any(|e| e == &parsed.name) {
            bail!(
                "artifact '{filename}' has pkgname '{}' not among expected {expected:?}",
                parsed.name
            );
        }
    }
    Ok(())
}

/// True for makepkg's split debug package of one of the `expected` pkgnames
/// (`<pkgname>-debug`), which the server discards rather than publishing.
///
/// AURCache serves a single flat repo per architecture, so a debug package would
/// appear in `pacman -Ss` right next to the real ones. Arch keeps them out of
/// `core`/`extra`/`multilib` for exactly that reason — they live in separate,
/// opt-in `*-debug` repos.
///
/// The server already forces `OPTIONS+=(!debug)` so these are not built at all.
/// This is the backstop for a worker whose user-supplied `makepkg.conf`
/// re-enables `debug`: without it, the extra artifact fails
/// [`validate_artifact_names`] and the build fails.
#[must_use]
fn is_debug_artifact(expected: &[String], filename: &str) -> bool {
    let Ok(parsed) = parse_arch_pkg(filename) else {
        return false;
    };
    parsed
        .name
        .strip_suffix("-debug")
        .is_some_and(|base| expected.iter().any(|e| e == base))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_pkg_filename() {
        let p = parse_arch_pkg("hello-2.12.1-1-x86_64.pkg.tar.zst").unwrap();
        assert_eq!(p.name, "hello");
        assert_eq!(p.version, "2.12.1-1");
    }

    #[test]
    fn parses_hyphenated_pkg_name() {
        let p = parse_arch_pkg("my-cool-pkg-1.0.0-2-aarch64.pkg.tar.zst").unwrap();
        assert_eq!(p.name, "my-cool-pkg");
        assert_eq!(p.version, "1.0.0-2");
    }

    #[test]
    fn a_package_name_can_contain_the_archive_delimiter() {
        let p = parse_arch_pkg("my.pkg.tar.tool-1.0-1-x86_64.pkg.tar.zst").unwrap();
        assert_eq!(p.name, "my.pkg.tar.tool");
        assert_eq!(p.version, "1.0-1");
    }

    #[test]
    fn rejects_malformed_filename() {
        assert!(parse_arch_pkg("garbage.pkg.tar.zst").is_err());
    }

    #[test]
    fn validate_accepts_expected_and_signatures() {
        let expected = vec!["hello".to_string(), "hello-docs".to_string()];
        let files = vec![
            "hello-1.0-1-x86_64.pkg.tar.zst".to_string(),
            "hello-1.0-1-x86_64.pkg.tar.zst.sig".to_string(),
            "hello-docs-1.0-1-x86_64.pkg.tar.zst".to_string(),
        ];
        assert!(validate_artifact_names(&expected, &files).is_ok());
    }

    #[test]
    fn validate_rejects_unexpected_pkgname() {
        let expected = vec!["hello".to_string()];
        let files = vec!["openssh-9.0-1-x86_64.pkg.tar.zst".to_string()];
        assert!(validate_artifact_names(&expected, &files).is_err());
    }

    #[test]
    fn validate_rejects_non_package_file() {
        let expected = vec!["hello".to_string()];
        let files = vec!["evil.sh".to_string()];
        assert!(validate_artifact_names(&expected, &files).is_err());
    }

    #[test]
    fn debug_artifact_detected_for_expected_package() {
        let expected = vec!["hello".to_string(), "hello-docs".to_string()];
        assert!(is_debug_artifact(
            &expected,
            "hello-debug-2.12.1-2-x86_64.pkg.tar.zst"
        ));
        assert!(is_debug_artifact(
            &expected,
            "hello-docs-debug-2.12.1-2-x86_64.pkg.tar.zst"
        ));
    }

    /// Only the debug split of an *expected* package is dropped; a foreign
    /// `-debug` upload must still reach `validate_artifact_names` and be rejected.
    #[test]
    fn debug_artifact_of_foreign_package_is_not_dropped() {
        let expected = vec!["hello".to_string()];
        let foreign = "openssh-debug-9.0-1-x86_64.pkg.tar.zst";
        assert!(!is_debug_artifact(&expected, foreign));
        assert!(validate_artifact_names(&expected, &[foreign.to_string()]).is_err());
    }

    #[test]
    fn regular_package_is_not_a_debug_artifact() {
        let expected = vec!["hello".to_string()];
        assert!(!is_debug_artifact(
            &expected,
            "hello-2.12.1-2-x86_64.pkg.tar.zst"
        ));
    }
}
