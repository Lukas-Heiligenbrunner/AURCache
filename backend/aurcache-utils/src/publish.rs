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
use anyhow::{anyhow, bail, ensure};
use aurcache_activitylog::activity_utils::ActivityLog;
use aurcache_activitylog::events::Event;
use aurcache_common::api::log::BuildRef;
use aurcache_common::build_state::BuildState;
use aurcache_db::helpers::builds::{Transition, refresh_package_status, transition};
use aurcache_db::helpers::time::now_secs;
use aurcache_db::prelude::{Builds, Dependencies, Files, Packages};
use aurcache_db::{builds, dependencies, files, packages};
use pacman_mirrors::platforms::Platform;
use pacman_repo_utils::{PackageEntry, PackageMeta};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, DatabaseConnection, DbErr, EntityTrait,
    FromQueryResult, PaginatorTrait, QueryFilter, QuerySelect, Set,
};
use std::collections::{BTreeSet, HashMap};
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
            repo.discard_staging(build_id).await;
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
    let named = Packages::find_by_id(build.pkg_id)
        .one(db)
        .await
        .ok()
        .flatten()
        .map(|pkg| BuildRef {
            pkgbase: pkg.name,
            number: build.number,
        });

    let publication = if build.status == BuildState::Successful {
        Ok(Publication::Earlier)
    } else if recorded(&build) {
        // An earlier attempt committed the files and the repository; only
        // the status did not follow. Reading every archive again would redo
        // all of that for nothing.
        settle(db, &build).await.map(|()| {
            info!("published build #{build_id}");
            Publication::Now
        })
    } else {
        publish(db, repo, &build).await.map(|()| Publication::Now)
    };
    let outcome = match publication {
        Ok(publication) => {
            // Announced once, by the call that published it: a retry to
            // promote dependents, or a resume after a restart, has nothing new
            // to say.
            if publication == Publication::Now {
                announce(activity, &build, named.as_ref()).await;
            }
            promote_dependents(db, activity, &build, named.as_ref()).await
        }
        Err(e)
            if e.downcast_ref::<crate::repository::FinalizationPending>()
                .is_some() =>
        {
            warn!("publication of build #{build_id} needs recovery: {e:#}");
            PublicationOutcome::Retry
        }
        Err(e) => report_failure(db, activity, &build, named.as_ref(), &e).await,
    };
    if outcome == PublicationOutcome::Finished {
        repo.discard_staging(build_id).await;
    }
    outcome
}

/// Tell the build's log and the activity log that `build` is published.
async fn announce(activity: &ActivityLog, build: &builds::Model, named: Option<&BuildRef>) {
    log(
        Some(activity),
        named.map(|b| b.pkgbase.as_str()),
        build.number,
        "Published to the repository\n",
    )
    .await;
    if let Some(published) = named {
        activity.emit(Event::BuildPublished {
            build: published.clone(),
            version: Some(build.version.clone()).filter(|v| !v.is_empty()),
        });
    }
}

/// Let the builds waiting on the published `build` go, where it was the last
/// thing they waited for. A failure is retried: nothing else would ever
/// promote them.
async fn promote_dependents(
    db: &DatabaseConnection,
    activity: &ActivityLog,
    build: &builds::Model,
    named: Option<&BuildRef>,
) -> PublicationOutcome {
    match crate::worker_complete::trigger_dependents(db, build.pkg_id, build.platform).await {
        Ok(unblocked) => {
            for unblocked in unblocked {
                activity.emit(Event::BuildUnblocked {
                    build: unblocked,
                    by: named.cloned(),
                });
            }
            PublicationOutcome::Finished
        }
        Err(e) => {
            error!(
                "Failed to trigger dependents of package {}: {e}",
                build.pkg_id
            );
            if let Some(named) = named {
                activity.emit(Event::DependentsTriggerFailed {
                    pkg: named.pkgbase.as_str().into(),
                    error: e.to_string(),
                });
            }
            PublicationOutcome::Retry
        }
    }
}

/// Record that `build` could not be published, and fail it. Only marking it
/// failed is retried: the failure itself is for good.
async fn report_failure(
    db: &DatabaseConnection,
    activity: &ActivityLog,
    build: &builds::Model,
    named: Option<&BuildRef>,
    e: &anyhow::Error,
) -> PublicationOutcome {
    let build_id = build.id;
    let pkgbase = named.map(|b| b.pkgbase.as_str());
    warn!("publishing build #{build_id} failed: {e:#}");
    // The build worked and the package exists; it is the last step that
    // did not. That is worth an entry someone will come across, rather
    // than only a line in this process's journal.
    activity.emit(Event::PublishFailed {
        build: BuildRef {
            pkgbase: pkgbase.unwrap_or("?").to_string(),
            number: build.number,
        },
        error: format!("{e:#}"),
    });
    log(
        Some(activity),
        pkgbase,
        build.number,
        &format!("Publishing failed: {e:#}\n"),
    )
    .await;
    if let Err(e) = fail(db, build).await {
        error!("could not mark build #{build_id} failed: {e}");
        if let Some(named) = named {
            activity.emit(Event::BuildMarkFailed {
                build: named.clone(),
                error: e.to_string(),
            });
        }
        return PublicationOutcome::Retry;
    }
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
#[derive(Clone, Copy, PartialEq, Eq)]
enum Publication {
    /// Published by this call.
    Now,
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
    /// What its filename says it is.
    named: ParsedPkg,
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
    /// produces, and another's of a name it takes over (see [`plan`]).
    stale: Vec<PublishedFile>,
    version: String,
    total_size: i64,
}

async fn publish(
    db: &DatabaseConnection,
    repo: &Repository,
    build: &builds::Model,
) -> anyhow::Result<()> {
    let pkg = Packages::find_by_id(build.pkg_id)
        .one(db)
        .await?
        .ok_or_else(|| anyhow!("the package is gone"))?;
    let platform = build.platform;
    let staged = read_staging(repo, build, &pkg).await?;
    let packages = staged.len();
    let version = staged[0].named.version.clone();

    // The slow part, and private: every archive read in full, before anything
    // is locked or changed.
    let mut described = Vec::with_capacity(staged.len());
    for Upload {
        path,
        filename,
        named,
    } in staged
    {
        let size = i64::try_from(tokio::fs::metadata(&path).await?.len()).unwrap_or(i64::MAX);
        let entry = repo
            .describe(path.clone())
            .await
            .map_err(|e| e.context(format!("reading {filename}")))?;
        check_contents(&filename, &named, &entry.meta)?;
        described.push(Staged {
            path,
            filename,
            entry,
            size,
        });
    }

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
    info!("published build #{}: {packages} package(s)", build.id);
    Ok(())
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
    transition(
        db,
        build,
        &[BuildState::Publishing],
        BuildState::Successful,
        |update| update,
    )
    .await
    .map(|_| ())
    .map_err(|error| anyhow::Error::new(crate::repository::FinalizationPending(error.into())))
}

/// The staged files that are to be published, by filename.
///
/// Detached signatures are left out -- the repository moves them with their
/// package -- and so are makepkg's `-debug` packages (see
/// [`Artifact::Debug`]). What remains must be named for this package.
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
        match artifact(&expected, &filename)? {
            Artifact::Debug => {
                log(
                    None,
                    Some(&pkg.name),
                    build.number,
                    &format!("skipping debug package (not published): {filename}\n"),
                )
                .await;
            }
            Artifact::Package(named) => names.push(Upload {
                path: entry.path(),
                filename,
                named,
            }),
        }
    }
    if names.is_empty() {
        bail!("no publishable artifacts were uploaded");
    }
    names.sort_by(|a, b| a.filename.cmp(&b.filename));
    Ok(names)
}

/// Decide who owns each file and what goes stale, from what `update` reads.
///
/// A package name another package already publishes on this platform is
/// refused, whatever its version: `repo.db` would list two packages of one
/// name (see [`standing`] for when another package gives it up, and when
/// both keep publishing it). A file of another package's that gives its name
/// up is taken over when this build stages that very file, and retired
/// otherwise.
async fn plan(
    update: &Update<'_>,
    db: &DatabaseConnection,
    pkg: &packages::Model,
    platform: Platform,
    described: &[Staged],
    version: String,
) -> anyhow::Result<Plan> {
    let pkgnames: Vec<String> = described
        .iter()
        .map(|staged| staged.entry.meta.pkgname.clone())
        .collect();
    let staged_names: Vec<&str> = described.iter().map(|s| s.filename.as_str()).collect();
    // What this platform already publishes under these names, in one query
    // rather than one per staged file (see `Update::published_files_named`).
    let named = update
        .published_files_named(db, platform, &pkgnames)
        .await?;
    let files: Vec<NamedFile<'_>> = named
        .iter()
        .map(|file| NamedFile {
            package_id: file.package_id(),
            filename: file.filename(),
        })
        .collect();
    let yielded = match standing(db, Some(pkg.id), &staged_names, &files).await? {
        Standing::Taken(taken) => bail!("{taken}"),
        Standing::Clear { yielded } => yielded,
    };

    let rows = described
        .iter()
        .map(|staged| FileRow {
            filename: staged.filename.clone(),
            existing: named
                .iter()
                .find(|file| file.filename() == staged.filename)
                .map(PublishedFile::id),
            size: staged.size,
        })
        .collect();

    let stale: Vec<PublishedFile> = update
        .published_files_of(db, &[pkg.id])
        .await?
        .into_iter()
        .filter(|file| file.platform() == platform && !staged_names.contains(&file.filename()))
        .chain(
            named
                .iter()
                .filter(|file| {
                    yielded.iter().any(|name| name == file.filename())
                        && !staged_names.contains(&file.filename())
                })
                .cloned(),
        )
        .collect();

    Ok(Plan {
        rows,
        stale,
        version,
        total_size: described.iter().map(|staged| staged.size).sum(),
    })
}

/// A package name another package publishes, which a build may not publish
/// as well.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Taken {
    /// The file it publishes it in.
    pub(crate) filename: String,
    /// That package.
    pub(crate) owner: String,
}

impl std::fmt::Display for Taken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name =
            parse_arch_pkg(&self.filename).map_or_else(|_| self.filename.clone(), |n| n.name);
        write!(
            f,
            "'{name}' is already published by the package '{}' ({}); \
             a package name is published by one package at a time",
            self.owner, self.filename
        )
    }
}

/// One published file, by the package publishing it.
pub(crate) struct NamedFile<'a> {
    pub(crate) package_id: i32,
    pub(crate) filename: &'a str,
}

/// What the files other packages publish under a build's package names leave
/// it.
pub(crate) enum Standing {
    /// One of those packages keeps the name: the build may not publish it.
    Taken(Taken),
    /// None does. The files `yielded` give their names up to the build.
    Clear { yielded: Vec<String> },
}

/// Decide whether a build of the package `pkg_id` may publish the names of
/// the files it stages, `staged`, given `named`: every package's files on its
/// platform under those names. `pkg_id` is `None` for a package not created
/// yet.
///
/// Another package's file keeps its name from the build, unless:
/// - that package no longer exists (its row is a leftover, not a claim);
/// - it depends on this one and its own source no longer builds the name:
///   the name moved from it to its dependency, and the build takes it over;
/// - the building package publishes the name too, in a file of another
///   version: two packages that both publish a name each keep publishing it,
///   so that neither is stopped by the other, while no third may start.
pub(crate) async fn standing<C: ConnectionTrait>(
    db: &C,
    pkg_id: Option<i32>,
    staged: &[&str],
    named: &[NamedFile<'_>],
) -> Result<Standing, DbErr> {
    let name_of = |filename: &str| parse_arch_pkg(filename).ok().map(|parsed| parsed.name);
    let own: Vec<String> = named
        .iter()
        .filter(|file| Some(file.package_id) == pkg_id)
        .filter_map(|file| name_of(file.filename))
        .collect();
    let mut yielded = Vec::new();
    for file in named.iter().filter(|file| Some(file.package_id) != pkg_id) {
        let Some(name) = name_of(file.filename) else {
            continue;
        };
        if own.contains(&name) && !staged.contains(&file.filename) {
            continue;
        }
        match holder(db, file.package_id, pkg_id, &name).await? {
            Holder::Holds(owner) => {
                return Ok(Standing::Taken(Taken {
                    filename: file.filename.to_string(),
                    owner,
                }));
            }
            Holder::Yields => yielded.push(file.filename.to_string()),
        }
    }
    Ok(Standing::Clear { yielded })
}

/// What the package `owner_id`, which publishes `name`, is to a build of
/// `pkg_id` that would publish it too.
enum Holder {
    /// It keeps the name.
    Holds(String),
    /// It gives the name up (see [`standing`]).
    Yields,
}

async fn holder<C: ConnectionTrait>(
    db: &C,
    owner_id: i32,
    pkg_id: Option<i32>,
    name: &str,
) -> Result<Holder, DbErr> {
    let Some(owner) = Packages::find_by_id(owner_id).one(db).await? else {
        return Ok(Holder::Yields);
    };
    let Some(pkg_id) = pkg_id else {
        return Ok(Holder::Holds(owner.name));
    };
    if expected_pkgnames(&owner).iter().any(|built| built == name) {
        return Ok(Holder::Holds(owner.name));
    }
    let depends_on_us = Dependencies::find()
        .filter(dependencies::Column::DependentId.eq(owner_id))
        .filter(dependencies::Column::DependeeId.eq(pkg_id))
        .count(db)
        .await?
        > 0;
    Ok(if depends_on_us {
        Holder::Yields
    } else {
        Holder::Holds(owner.name)
    })
}

/// A package name more than one package publishes on one platform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedName {
    pub name: String,
    pub platform: Platform,
    /// The packages publishing it, by name, in order.
    pub packages: Vec<String>,
}

/// The package names more than one package publishes on one platform.
///
/// Publishing lets none start publishing a name another package does, but
/// lets the packages already sharing one go on (see [`standing`]): this is
/// how to find them, which only removing all but one of them resolves.
///
/// # Errors
/// The `files` table could not be read.
pub async fn shared_names<C: ConnectionTrait>(db: &C) -> Result<Vec<SharedName>, DbErr> {
    #[derive(FromQueryResult)]
    struct Row {
        filename: String,
        platform: Platform,
        pkgbase: String,
    }
    let rows = Files::find()
        .select_only()
        .column(files::Column::Filename)
        .column(files::Column::Platform)
        .column_as(packages::Column::Name, "pkgbase")
        .inner_join(Packages)
        .into_model::<Row>()
        .all(db)
        .await?;
    let mut publishers: HashMap<(Platform, String), BTreeSet<String>> = HashMap::new();
    for row in rows {
        if let Ok(parsed) = parse_arch_pkg(&row.filename) {
            publishers
                .entry((row.platform, parsed.name))
                .or_default()
                .insert(row.pkgbase);
        }
    }
    let mut shared: Vec<SharedName> = publishers
        .into_iter()
        .filter(|(_, packages)| packages.len() > 1)
        .map(|((platform, name), packages)| SharedName {
            name,
            platform,
            packages: packages.into_iter().collect(),
        })
        .collect();
    shared
        .sort_by(|a, b| (a.platform.to_string(), &a.name).cmp(&(b.platform.to_string(), &b.name)));
    Ok(shared)
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
///
/// A build that has left `PUBLISHING` meanwhile -- cancelled, or deleted -- is
/// left as it is.
async fn fail(db: &DatabaseConnection, build: &builds::Model) -> Result<Transition, DbErr> {
    transition(
        db,
        build,
        &[BuildState::Publishing],
        BuildState::Failed,
        |update| update.col_expr(builds::Column::EndTime, Some(now_secs()).into()),
    )
    .await
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
    for name in aurcache_db::lists::JsonList::entries(pkg.split_packages.as_ref()) {
        if !names.contains(name) {
            names.push(name.clone());
        }
    }
    names
}

/// What a package's filename says it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedPkg {
    pub(crate) name: String,
    /// `[epoch:]pkgver-pkgrel`.
    pub(crate) version: String,
    pub(crate) arch: String,
}

/// Parse an Arch package filename into its name, version and architecture.
///
/// e.g. `hello-2.12.1-1-x86_64.pkg.tar.zst` -> name `hello`, version
/// `2.12.1-1`, arch `x86_64`.
pub(crate) fn parse_arch_pkg(filename: &str) -> anyhow::Result<ParsedPkg> {
    let (base, _) = filename
        .rsplit_once(".pkg.tar.")
        .ok_or_else(|| anyhow!("Invalid pkg filename: {filename}"))?;

    let parts: Vec<&str> = base.split('-').collect();
    if parts.len() < 4 {
        bail!("Invalid pkg filename format: {filename}");
    }

    let arch = parts[parts.len() - 1];
    let pkgrel = parts[parts.len() - 2];
    let pkgver = parts[parts.len() - 3];
    let name = parts[..parts.len() - 3].join("-");

    Ok(ParsedPkg {
        name,
        version: format!("{pkgver}-{pkgrel}"),
        arch: arch.to_string(),
    })
}

/// Check that a package is what its filename says: the same pkgname, version
/// and architecture in its `.PKGINFO`.
///
/// The filename is what is checked against the package names a build may
/// produce, but the `repo.db` entry is made from the `.PKGINFO`. Without this,
/// `hello-1.0-1-x86_64.pkg.tar.zst` holding `pkgname = openssh` would publish
/// an `openssh` entry under `hello`'s name. An architecture the `.PKGINFO`
/// does not state is not held against it.
pub(crate) fn check_contents(
    filename: &str,
    named: &ParsedPkg,
    meta: &PackageMeta,
) -> anyhow::Result<()> {
    ensure!(
        meta.pkgname == named.name,
        "{filename} holds pkgname '{}', not '{}'",
        meta.pkgname,
        named.name
    );
    ensure!(
        meta.version == named.version,
        "{filename} holds version '{}', not '{}'",
        meta.version,
        named.version
    );
    if let Some(arch) = &meta.arch {
        ensure!(
            *arch == named.arch,
            "{filename} holds arch '{arch}', not '{}'",
            named.arch
        );
    }
    Ok(())
}

/// What a staged package file is to publishing.
#[derive(Debug)]
pub(crate) enum Artifact {
    /// makepkg's split debug package of one of the expected pkgnames
    /// (`<pkgname>-debug`), which the server discards rather than publishing.
    ///
    /// AURCache serves a single flat repo per architecture, so a debug package
    /// would appear in `pacman -Ss` right next to the real ones. Arch keeps
    /// them out of `core`/`extra`/`multilib` for exactly that reason -- they
    /// live in separate, opt-in `*-debug` repos.
    ///
    /// The server already forces `OPTIONS+=(!debug)` so these are not built at
    /// all. This is the backstop for a worker whose user-supplied
    /// `makepkg.conf` re-enables `debug`: without it, the extra artifact is
    /// not among the expected names and the build fails.
    Debug,
    /// A package to publish.
    Package(ParsedPkg),
}

/// Check the staged package `filename` against the package names the server
/// expects for this package (its pkgbase plus any split packages; see
/// [`expected_pkgnames`]).
///
/// This blocks *wrong-named* artifacts (e.g. a worker uploading `openssh-*`
/// under a job for `hello`); it does **not** and cannot verify the *contents*
/// of a correctly-named package (see design non-goals). A file that is not a
/// `*.pkg.tar.*` at all is refused too.
pub(crate) fn artifact(expected: &[String], filename: &str) -> anyhow::Result<Artifact> {
    let parsed = parse_arch_pkg(filename)?;
    if parsed
        .name
        .strip_suffix("-debug")
        .is_some_and(|base| expected.iter().any(|e| e == base))
    {
        return Ok(Artifact::Debug);
    }
    ensure!(
        expected.contains(&parsed.name),
        "artifact '{filename}' has pkgname '{}' not among expected {expected:?}",
        parsed.name
    );
    Ok(Artifact::Package(parsed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_pkg_filename() {
        let p = parse_arch_pkg("hello-2.12.1-1-x86_64.pkg.tar.zst").unwrap();
        assert_eq!(p.name, "hello");
        assert_eq!(p.version, "2.12.1-1");
        assert_eq!(p.arch, "x86_64");
    }

    fn meta(pkgname: &str, version: &str, arch: Option<&str>) -> PackageMeta {
        PackageMeta {
            pkgname: pkgname.to_string(),
            version: version.to_string(),
            arch: arch.map(str::to_string),
        }
    }

    #[test]
    fn contents_must_match_the_filename() {
        let filename = "hello-1:2.0-1-x86_64.pkg.tar.zst";
        let named = parse_arch_pkg(filename).unwrap();
        assert!(
            check_contents(filename, &named, &meta("hello", "1:2.0-1", Some("x86_64"))).is_ok()
        );
        assert!(check_contents(filename, &named, &meta("hello", "1:2.0-1", None)).is_ok());
        assert!(check_contents(filename, &named, &meta("openssh", "1:2.0-1", None)).is_err());
        assert!(check_contents(filename, &named, &meta("hello", "2.0-1", None)).is_err());
        assert!(check_contents(filename, &named, &meta("hello", "1:2.0-1", Some("any"))).is_err());
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

    fn is_package(expected: &[String], filename: &str) -> bool {
        matches!(artifact(expected, filename), Ok(Artifact::Package(_)))
    }

    fn is_debug(expected: &[String], filename: &str) -> bool {
        matches!(artifact(expected, filename), Ok(Artifact::Debug))
    }

    #[test]
    fn the_pkgbase_and_its_split_packages_are_published() {
        let expected = vec!["hello".to_string(), "hello-docs".to_string()];
        assert!(is_package(&expected, "hello-1.0-1-x86_64.pkg.tar.zst"));
        assert!(is_package(&expected, "hello-docs-1.0-1-x86_64.pkg.tar.zst"));
    }

    #[test]
    fn an_unexpected_pkgname_is_refused() {
        let expected = vec!["hello".to_string()];
        assert!(artifact(&expected, "openssh-9.0-1-x86_64.pkg.tar.zst").is_err());
    }

    #[test]
    fn a_non_package_file_is_refused() {
        let expected = vec!["hello".to_string()];
        assert!(artifact(&expected, "evil.sh").is_err());
    }

    #[test]
    fn debug_artifact_detected_for_expected_package() {
        let expected = vec!["hello".to_string(), "hello-docs".to_string()];
        assert!(is_debug(
            &expected,
            "hello-debug-2.12.1-2-x86_64.pkg.tar.zst"
        ));
        assert!(is_debug(
            &expected,
            "hello-docs-debug-2.12.1-2-x86_64.pkg.tar.zst"
        ));
    }

    /// Only the debug split of an *expected* package is dropped; a foreign
    /// `-debug` upload is refused like any other unexpected name.
    #[test]
    fn debug_artifact_of_foreign_package_is_refused() {
        let expected = vec!["hello".to_string()];
        assert!(artifact(&expected, "openssh-debug-9.0-1-x86_64.pkg.tar.zst").is_err());
    }

    #[test]
    fn regular_package_is_not_a_debug_artifact() {
        let expected = vec!["hello".to_string()];
        assert!(is_package(&expected, "hello-2.12.1-2-x86_64.pkg.tar.zst"));
    }
}
