//! The pacman repository this server publishes: the package files, the
//! `repo.db`/`repo.files` databases indexing them, the `files` rows recording
//! who publishes what, and the staging area uploads wait in. Every change to it
//! goes through [`Repository::begin`].

use aurcache_activitylog::activity_utils::ActivityLog;
use aurcache_activitylog::events::{Event, FileAction};
use aurcache_common::repo::REPO_NAME;
use aurcache_db::files;
use aurcache_db::prelude::Files;
use pacman_mirrors::platforms::Platform;
use pacman_repo_utils::PackageEntry;
use sea_orm::{
    ColumnTrait, ConnectionTrait, DatabaseConnection, DatabaseTransaction, EntityTrait,
    QueryFilter, TransactionTrait,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tracing::warn;

pub use aurcache_common::fs::REPO_ROOT;

/// Where uploads wait to be published, under the repository root.
///
/// Dot-named, and the repository's file server refuses dot segments, so an
/// upload is never downloadable before it is published.
const STAGING_DIR: &str = ".staging";

/// `<REPO_NAME>.db.tar.gz`; a test keeps the two in step.
const DB_ARCHIVE: &str = "repo.db.tar.gz";
/// `<REPO_NAME>.files.tar.gz`, likewise.
const FILES_ARCHIVE: &str = "repo.files.tar.gz";

/// How many times a failed `commit` is tried in all. Enough to ride out a
/// database that is briefly busy; a failure that will not go away just fails a
/// few times quickly.
const COMMIT_ATTEMPTS: u32 = 3;

/// The repository on disk.
///
/// One per server. Every change is serialized by its lock, covering the whole
/// read-modify-write of the databases: two publishes on the same platform each
/// starting from the databases as they were would each drop the other's entry
/// when renaming its own result in, however atomic the rename.
pub struct Repository {
    root: PathBuf,
    lock: Arc<tokio::sync::Mutex<()>>,
    /// Numbers each update, so a [`PublishedFile`] can say which one read it.
    updates: AtomicU64,
    /// When each retired file stopped being listed, as far as this process
    /// knows: set by the update that retired it, or by the first sweep that
    /// found it unlisted. Kept in memory only -- after a restart a retired
    /// file's grace period starts again, which can only keep it longer.
    retired: std::sync::Mutex<HashMap<PathBuf, Instant>>,
    /// Where a file that would not move, a commit that had to be retried, and
    /// what the sweep removed are recorded. Discarding until the server hands
    /// it a real one ([`Self::with_log`]).
    log: ActivityLog,
}

/// A package file the repository publishes, as its `files` row records it.
///
/// Only an [`Update`] reads these, and [`Update::retire`] only takes these, from
/// the same update: a file is only ever retired on the strength of a read made
/// under the repository lock, by the change doing the retiring. A read made
/// before the lock, or by an earlier change, could be out of date -- another
/// package may have claimed the file since -- and acting on it would retire that
/// package's file.
#[derive(Clone, Debug)]
pub struct PublishedFile {
    update: u64,
    row: files::Model,
}

impl PublishedFile {
    /// The `files` row's id.
    #[must_use]
    pub fn id(&self) -> i32 {
        self.row.id
    }

    #[must_use]
    pub fn filename(&self) -> &str {
        &self.row.filename
    }

    #[must_use]
    pub fn platform(&self) -> Platform {
        self.row.platform
    }

    /// The package publishing it.
    #[must_use]
    pub fn package_id(&self) -> i32 {
        self.row.package_id
    }
}

/// What one update adds and retires.
#[derive(Default)]
struct Changes {
    add: Vec<Addition>,
    retire: Vec<Retirement>,
}

/// A published file an update takes out of the databases.
struct Retirement {
    platform: Platform,
    filename: String,
}

struct Addition {
    platform: Platform,
    staged: PathBuf,
    entry: PackageEntry,
}

impl Changes {
    fn add(&mut self, platform: Platform, staged: PathBuf, entry: PackageEntry) {
        self.add.push(Addition {
            platform,
            staged,
            entry,
        });
    }

    fn retire(&mut self, platform: Platform, filename: impl Into<String>) {
        self.retire.push(Retirement {
            platform,
            filename: filename.into(),
        });
    }

    fn platforms(&self) -> Vec<Platform> {
        let mut platforms: Vec<Platform> = self
            .add
            .iter()
            .map(|a| a.platform)
            .chain(self.retire.iter().map(|r| r.platform))
            .collect();
        platforms.sort_by_key(Platform::as_str);
        platforms.dedup();
        platforms
    }
}

/// One change to the repository, holding its lock (the `_lock` field). See
/// [`Repository::begin`].
///
/// Reading what the repository publishes, to decide what to change, goes
/// through this too: the reads are only available while the lock is held.
pub struct Update<'a> {
    repo: &'a Repository,
    /// Which update this is, so a [`PublishedFile`] read by another one is
    /// refused.
    id: u64,
    /// The repository lock: the guard of [`Repository`]'s mutex, taken by
    /// [`Repository::begin`] and released when the update is committed or
    /// dropped. Never read, only held -- hence the underscore.
    ///
    /// It is exclusive *write* access to the repository, which is three things
    /// kept in step:
    ///
    /// - each platform's `repo.db` and `repo.files`, rewritten as a whole from
    ///   their current contents (and through fixed `.next` names beside them);
    /// - the package files in each platform directory;
    /// - the `files` rows saying which package publishes which of those files.
    ///
    /// So anything that changes one of these holds it, and so does anything
    /// that *decides* a change from what they currently say -- which is why the
    /// `files` reads a change is based on are only available from an update.
    /// Without it, two changes each start from the same state and the second
    /// undoes the first: an entry dropped from `repo.db`, or a file removed on
    /// the strength of a row another package has since claimed.
    ///
    /// Reading needs no lock, because every write is atomic from a reader's
    /// side: a database is replaced by renaming a complete new one over it, a
    /// package file is renamed into place, and the `files` rows change in one
    /// transaction. A client or the web UI sees the repository before a change
    /// or after it. Order does the rest: new package files land before the
    /// databases listing them, and a retired file stays downloadable for a
    /// grace period after the databases stop listing it, for clients still
    /// holding an older `repo.db` (see [`Repository::sweep`]).
    ///
    /// Not covered: the staging directories, which each belong to one build
    /// and are written by its uploads without the lock; every other table; and
    /// anything outside this process -- it is an in-process mutex, so a second
    /// server on the same repository, or a hand edit, is not serialized.
    _lock: Arc<tokio::sync::OwnedMutexGuard<()>>,
    /// What the change adds and retires, applied by [`Update::commit`].
    changes: Changes,
}

impl Update<'_> {
    /// Which of `filenames` are already published on `platform`, keyed by
    /// filename.
    ///
    /// One query rather than one per file: a split package publishes several
    /// outputs, and planning runs under the repository lock, where every
    /// round trip lengthens the critical section. Stamped with this update
    /// like the single-file read, so the retire discipline is unchanged.
    pub async fn published_files<C: ConnectionTrait>(
        &self,
        db: &C,
        platform: Platform,
        filenames: &[String],
    ) -> anyhow::Result<HashMap<String, PublishedFile>> {
        if filenames.is_empty() {
            return Ok(HashMap::new());
        }
        let rows = Files::find()
            .filter(files::Column::Filename.is_in(filenames.iter().map(String::as_str)))
            .filter(files::Column::Platform.eq(platform))
            .all(db)
            .await?;
        Ok(rows
            .into_iter()
            .map(|row| (row.filename.clone(), self.published(row)))
            .collect())
    }

    /// Every file the packages `pkg_ids` publish, on every platform.
    pub async fn published_files_of<C: ConnectionTrait>(
        &self,
        db: &C,
        pkg_ids: &[i32],
    ) -> anyhow::Result<Vec<PublishedFile>> {
        let rows = Files::find()
            .filter(files::Column::PackageId.is_in(pkg_ids.iter().copied()))
            .all(db)
            .await?;
        Ok(rows.into_iter().map(|row| self.published(row)).collect())
    }

    /// Every file the repository publishes.
    pub async fn all_published_files<C: ConnectionTrait>(
        &self,
        db: &C,
    ) -> anyhow::Result<Vec<PublishedFile>> {
        let rows = Files::find().all(db).await?;
        Ok(rows.into_iter().map(|row| self.published(row)).collect())
    }

    /// The recorded changes, leaving none behind, for [`Update::commit`] to
    /// apply.
    fn take_changes(&mut self) -> Changes {
        std::mem::take(&mut self.changes)
    }

    fn published(&self, row: files::Model) -> PublishedFile {
        PublishedFile {
            update: self.id,
            row,
        }
    }

    /// Publish the package file `entry` describes, moving it from `staged`.
    pub fn add(&mut self, platform: Platform, staged: PathBuf, entry: PackageEntry) {
        self.changes.add(platform, staged, entry);
    }

    /// Take a published file out of the repository.
    ///
    /// Its database entry goes with this update. The file itself, and its
    /// detached signature, stay downloadable until [`Repository::sweep`]
    /// finds them unlisted for longer than the grace period: a client that
    /// synced just before this update still asks for them.
    ///
    /// Refused for a file this update did not read itself; see
    /// [`PublishedFile`].
    pub fn retire(&mut self, file: &PublishedFile) -> anyhow::Result<()> {
        if file.update != self.id {
            anyhow::bail!(
                "{} was read by another repository update; only the update that read it may retire it",
                file.filename()
            );
        }
        self.changes.retire(file.platform(), file.filename());
        Ok(())
    }

    /// Make the recorded changes, with `commit` -- the caller's database
    /// transaction -- as the decision point:
    ///
    /// Prepare replacements privately, commit the caller's changes and the
    /// recovery plan in one SQL transaction, then finalize the filesystem.
    /// A post-commit error leaves the plan for recovery before the next update.
    pub async fn commit<T, F>(mut self, db: &DatabaseConnection, mut commit: F) -> anyhow::Result<T>
    where
        F: for<'txn> FnMut(
            &'txn DatabaseTransaction,
        ) -> std::pin::Pin<
            Box<dyn Future<Output = anyhow::Result<T>> + Send + 'txn>,
        >,
        T: Send,
    {
        let root = self.repo.root().to_path_buf();
        let changes = Arc::new(self.take_changes());
        if let Some(addition) = changes.add.iter().find(|a| !a.staged.starts_with(&root)) {
            anyhow::bail!(
                "{} is not staged under {}",
                addition.staged.display(),
                root.display()
            );
        }
        let plan = {
            let root = root.clone();
            let changes = Arc::clone(&changes);
            let lock = Arc::clone(&self._lock);
            tokio::task::spawn_blocking(move || {
                let _lock = lock;
                let prepared = prepare(&root, &changes)?;
                Finalization::prepare(&root, &changes, &prepared)
            })
            .await??
        };
        let serialized = match serde_json::to_string(&plan) {
            Ok(serialized) => serialized,
            Err(error) => {
                plan.discard();
                return Err(error.into());
            }
        };
        let mut attempt = 1;
        let committed = loop {
            let result = async {
                let txn = db.begin().await?;
                let done = commit(&txn).await?;
                txn.execute(&plan_row::insert(&serialized)).await?;
                if let Err(error) = txn.commit().await {
                    // A failed COMMIT usually committed nothing (SQLite busy),
                    // but a connection lost after sending it may have committed
                    // everything. The plan row tells the two apart; when even
                    // that cannot be read, assume the worst and keep the plan.
                    return match plan_row::read(db).await {
                        Ok(Some(recorded)) if recorded == serialized => Ok(done),
                        Ok(_) => Err(error.into()),
                        Err(read) => Err(anyhow::Error::new(FinalizationPending(
                            anyhow::Error::new(error)
                                .context(format!("and the plan could not be read back: {read}")),
                        ))),
                    };
                }
                Ok::<_, anyhow::Error>(done)
            }
            .await;
            match result {
                Ok(done) => break done,
                Err(error) if error.downcast_ref::<FinalizationPending>().is_some() => {
                    return Err(error);
                }
                Err(error) if attempt < COMMIT_ATTEMPTS => {
                    self.repo.log.emit(Event::RepoCommitRetried {
                        attempt,
                        error: format!("{error:#}"),
                    });
                    tokio::time::sleep(Duration::from_secs(u64::from(attempt))).await;
                    attempt += 1;
                }
                Err(error) => {
                    plan.discard();
                    return Err(error);
                }
            }
        };
        self.repo
            .finalize(db, plan, Arc::clone(&self._lock))
            .await
            .map_err(|error| anyhow::Error::new(FinalizationPending(error)))?;
        Ok(committed)
    }
}

/// The one row of `repository_updates`: the plan of an update whose SQL has
/// committed and whose files may not all be in place yet.
mod plan_row {
    use sea_orm::sea_query::{
        Alias, DeleteStatement, Expr, ExprTrait, InsertStatement, Query, SelectStatement,
    };
    use sea_orm::{ConnectionTrait, DbErr};

    /// There is at most one plan at a time, so it always has this id.
    const ID: i32 = 1;

    fn table() -> Alias {
        Alias::new("repository_updates")
    }

    pub(super) fn insert(plan: &str) -> InsertStatement {
        Query::insert()
            .into_table(table())
            .columns([Alias::new("id"), Alias::new("plan")])
            .values_panic([ID.into(), plan.into()])
            .to_owned()
    }

    fn select() -> SelectStatement {
        Query::select()
            .column(Alias::new("plan"))
            .from(table())
            .and_where(Expr::col(Alias::new("id")).eq(ID))
            .to_owned()
    }

    pub(super) fn delete() -> DeleteStatement {
        Query::delete()
            .from_table(table())
            .and_where(Expr::col(Alias::new("id")).eq(ID))
            .to_owned()
    }

    /// The recorded plan, serialized, if there is one.
    pub(super) async fn read<C: ConnectionTrait>(db: &C) -> Result<Option<String>, DbErr> {
        db.query_one(&select())
            .await?
            .map(|row| row.try_get::<String>("", "plan"))
            .transpose()
    }
}

/// The new databases for one platform, written beside the current ones.
struct Prepared {
    db: PathBuf,
    db_next: PathBuf,
    files: PathBuf,
    files_next: PathBuf,
}

impl Prepared {
    fn discard(&self) {
        let _ = std::fs::remove_file(&self.db_next);
        let _ = std::fs::remove_file(&self.files_next);
    }
}

impl Repository {
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            lock: Arc::new(tokio::sync::Mutex::new(())),
            updates: AtomicU64::new(0),
            retired: std::sync::Mutex::new(HashMap::new()),
            log: ActivityLog::discarding(),
        }
    }

    /// Create the databases of every platform that has none yet.
    pub fn init(&self) -> anyhow::Result<()> {
        for platform in Platform::ALL {
            pacman_repo_utils::repo_init::init_repo(
                &Self::platform_dir(&self.root, platform),
                REPO_NAME,
            )?;
        }
        Ok(())
    }

    /// Record what the repository does, and what goes wrong with it, to `log`.
    #[must_use]
    pub fn with_log(mut self, log: ActivityLog) -> Self {
        self.log = log;
        self
    }

    /// Where the repository records what it does.
    #[must_use]
    pub fn log(&self) -> &ActivityLog {
        &self.log
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Where a build's artifacts are uploaded to and wait until published.
    ///
    /// Inside the repository root by construction: publishing moves the files
    /// into place with a rename, which needs both ends on one filesystem, and
    /// deployments mount the repository as a volume of its own.
    #[must_use]
    pub fn staging_dir(&self, build_id: i32) -> PathBuf {
        self.staging_root().join(build_id.to_string())
    }

    /// The directory every build's [`Repository::staging_dir`] is in.
    #[must_use]
    pub fn staging_root(&self) -> PathBuf {
        self.root.join(STAGING_DIR)
    }

    /// Read a staged package into its database entry. The slow part of
    /// publishing -- the whole archive is read -- and it changes nothing, so it
    /// needs no lock and belongs before [`Repository::begin`].
    pub async fn describe(&self, staged: PathBuf) -> anyhow::Result<PackageEntry> {
        tokio::task::spawn_blocking(move || pacman_repo_utils::describe_package(&staged)).await?
    }

    /// Start changing the repository.
    ///
    /// Takes the repository lock, which the returned update holds until it is
    /// committed or dropped. What the repository publishes is read through the
    /// update, so every decision about what to change is made with no other
    /// change able to interleave. Dropping it without committing changes
    /// nothing.
    pub async fn begin(&self, db: &DatabaseConnection) -> anyhow::Result<Update<'_>> {
        let lock = Arc::new(Arc::clone(&self.lock).lock_owned().await);
        self.recover_locked(db, Arc::clone(&lock)).await?;
        Ok(Update {
            repo: self,
            id: self.updates.fetch_add(1, Ordering::Relaxed),
            _lock: lock,
            changes: Changes::default(),
        })
    }

    /// Finish a previously committed update before accepting more repository work.
    pub async fn recover(&self, db: &DatabaseConnection) -> anyhow::Result<()> {
        let lock = Arc::new(Arc::clone(&self.lock).lock_owned().await);
        self.recover_locked(db, lock).await
    }

    async fn recover_locked(
        &self,
        db: &DatabaseConnection,
        lock: Arc<tokio::sync::OwnedMutexGuard<()>>,
    ) -> anyhow::Result<()> {
        if let Some(serialized) = plan_row::read(db).await? {
            let plan: Finalization = serde_json::from_str(&serialized)?;
            self.finalize(db, plan, lock)
                .await
                .map_err(|error| anyhow::Error::new(FinalizationPending(error)))?;
        }
        Ok(())
    }

    async fn finalize(
        &self,
        db: &DatabaseConnection,
        plan: Finalization,
        lock: Arc<tokio::sync::OwnedMutexGuard<()>>,
    ) -> anyhow::Result<()> {
        let log = self.log.clone();
        let task_lock = Arc::clone(&lock);
        let retired = tokio::task::spawn_blocking(move || {
            let _lock = task_lock;
            plan.finish(&log)
        })
        .await??;
        db.execute(&plan_row::delete()).await?;
        self.note_retired(retired, Instant::now());
        Ok(())
    }

    /// Delete the retired package files -- present in a platform directory, not
    /// listed by its `repo.db` -- that have been unlisted for at least
    /// `grace`. Returns how many files it deleted.
    ///
    /// Unlisted by `repo.db` rather than missing from the `files` table: if the
    /// database were ever wrong wholesale (restored from an old dump, or the
    /// wrong one), every file would look unpublished and the whole repository
    /// would go. `repo.db` lives beside the files and only ever changes through
    /// an update, atomically. A platform directory without a readable
    /// `repo.db` is skipped for the same reason.
    ///
    /// Takes the repository lock: deciding what to delete from what is listed
    /// is a change like any other, and an update in flight may be about to list
    /// a file this would otherwise see as unlisted.
    pub async fn sweep(&self, db: &DatabaseConnection, grace: Duration) -> anyhow::Result<usize> {
        self.sweep_at(db, Instant::now(), grace).await
    }

    async fn sweep_at(
        &self,
        db: &DatabaseConnection,
        now: Instant,
        grace: Duration,
    ) -> anyhow::Result<usize> {
        let lock = Arc::new(Arc::clone(&self.lock).lock_owned().await);
        self.recover_locked(db, Arc::clone(&lock)).await?;
        let root = self.root.clone();
        let known = std::mem::take(&mut *self.retired_times());
        let log = self.log.clone();
        let task_lock = Arc::clone(&lock);
        let Swept {
            still_retired,
            deleted,
        } = tokio::task::spawn_blocking(move || {
            let _lock = task_lock;
            sweep_retired(&root, known, now, grace, &log)
        })
        .await?;
        *self.retired_times() = still_retired;
        if !deleted.is_empty() {
            self.log.emit(Event::RepoSwept {
                files: deleted.clone(),
            });
        }
        Ok(deleted.len())
    }

    fn note_retired(&self, paths: Vec<PathBuf>, when: Instant) {
        let mut retired = self.retired_times();
        for path in paths {
            retired.insert(path, when);
        }
    }

    fn retired_times(&self) -> std::sync::MutexGuard<'_, HashMap<PathBuf, Instant>> {
        self.retired
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn platform_dir(root: &Path, platform: Platform) -> PathBuf {
        root.join(platform.as_str())
    }
}

/// Step 1: the new databases of every platform the changes touch, beside the
/// current ones. Dot-named, so never served; discarded if anything fails.
fn prepare(root: &Path, changes: &Changes) -> anyhow::Result<Vec<Prepared>> {
    let mut prepared = Vec::new();
    for platform in changes.platforms() {
        let dir = Repository::platform_dir(root, platform);
        let result = (|| {
            std::fs::create_dir_all(&dir)?;
            let p = Prepared {
                db: dir.join(DB_ARCHIVE),
                db_next: dir.join(format!(".{DB_ARCHIVE}.next")),
                files: dir.join(FILES_ARCHIVE),
                files_next: dir.join(format!(".{FILES_ARCHIVE}.next")),
            };
            let remove: Vec<String> = changes
                .retire
                .iter()
                .filter(|r| r.platform == platform)
                .map(|r| r.filename.clone())
                .collect();
            let add: Vec<PackageEntry> = changes
                .add
                .iter()
                .filter(|a| a.platform == platform)
                .map(|a| a.entry.clone())
                .collect();
            let written = pacman_repo_utils::write_updated_databases(
                &p.db,
                &p.files,
                &remove,
                &add,
                &p.db_next,
                &p.files_next,
            );
            if let Err(e) = written {
                p.discard();
                return Err(e);
            }
            Ok(p)
        })();
        match result {
            Ok(p) => prepared.push(p),
            Err(e) => {
                for p in &prepared {
                    p.discard();
                }
                return Err(e.context(format!("preparing the {platform} databases")));
            }
        }
    }
    Ok(prepared)
}

/// A durable filesystem plan, recorded in the same transaction as its SQL changes.
#[derive(Serialize, Deserialize)]
struct Finalization {
    moves: Vec<FileMove>,
    retired: Vec<PathBuf>,
}

#[derive(Serialize, Deserialize)]
struct FileMove {
    source: PathBuf,
    destination: PathBuf,
}

impl Finalization {
    fn prepare(root: &Path, changes: &Changes, prepared: &[Prepared]) -> anyhow::Result<Self> {
        let mut plan = Self {
            moves: Vec::new(),
            retired: Vec::new(),
        };
        let result = (|| {
            let mut added = HashSet::new();
            for addition in &changes.add {
                let filename = addition.entry.filename.as_str();
                let dir = Repository::platform_dir(root, addition.platform);
                added.insert((addition.platform, filename));
                // Preserve uploads until publication (including dependent promotion)
                // finishes. Recovery can then safely retry a still-PUBLISHING build.
                for source in [
                    addition.staged.clone(),
                    with_suffix(&addition.staged, ".sig"),
                ] {
                    if source != addition.staged && !source.exists() {
                        continue;
                    }
                    let pending = with_suffix(&source, ".publishing");
                    if pending.exists() {
                        std::fs::remove_file(&pending)?;
                    }
                    link_or_copy(&source, &pending)?;
                    let destination = if source == addition.staged {
                        dir.join(filename)
                    } else {
                        dir.join(format!("{filename}.sig"))
                    };
                    plan.moves.push(FileMove {
                        source: pending.clone(),
                        destination,
                    });
                    std::fs::File::open(&pending)?.sync_all()?;
                    std::fs::File::open(pending.parent().expect("staged sibling"))?.sync_all()?;
                }
            }
            for p in prepared {
                for (source, destination) in [(&p.db_next, &p.db), (&p.files_next, &p.files)] {
                    std::fs::File::open(source)?.sync_all()?;
                    std::fs::File::open(source.parent().expect("database sibling"))?.sync_all()?;
                    plan.moves.push(FileMove {
                        source: source.clone(),
                        destination: destination.clone(),
                    });
                }
            }
            for Retirement { platform, filename } in &changes.retire {
                if added.contains(&(*platform, filename.as_str())) {
                    continue;
                }
                let path = Repository::platform_dir(root, *platform).join(filename);
                plan.retired.push(with_suffix(&path, ".sig"));
                plan.retired.push(path);
            }
            Ok::<_, anyhow::Error>(())
        })();
        if let Err(error) = result {
            plan.discard();
            for p in prepared {
                p.discard();
            }
            return Err(error);
        }
        Ok(plan)
    }

    /// Remove replacements that have never been committed to SQL.
    fn discard(&self) {
        for movement in &self.moves {
            let _ = std::fs::remove_file(&movement.source);
        }
    }

    fn finish(self, log: &ActivityLog) -> anyhow::Result<Vec<PathBuf>> {
        for movement in &self.moves {
            match std::fs::rename(&movement.source, &movement.destination) {
                Ok(()) => {}
                // A restart replays the whole plan, including moves already done.
                Err(error)
                    if error.kind() == std::io::ErrorKind::NotFound
                        && !movement.source.exists()
                        && movement.destination.is_file() => {}
                // Not done, and never can be: what was to be moved is gone
                // (staging cleared by hand, a volume reset). Retrying would
                // block every later update for good, so the move is reported
                // and skipped; the build that produced it has to run again.
                Err(error)
                    if error.kind() == std::io::ErrorKind::NotFound
                        && !movement.source.exists() =>
                {
                    log.emit(Event::RepoFileFailed {
                        action: FileAction::Move,
                        path: movement.source.display().to_string(),
                        error: format!("{error}; skipped, the file is gone"),
                    });
                    continue;
                }
                Err(error) => {
                    log.emit(Event::RepoFileFailed {
                        action: FileAction::Move,
                        path: movement.source.display().to_string(),
                        error: error.to_string(),
                    });
                    return Err(error.into());
                }
            }
            if let Some(parent) = movement.destination.parent() {
                std::fs::File::open(parent)?.sync_all()?;
            }
        }
        Ok(self.retired)
    }
}

/// SQL has committed; the filesystem plan must be replayed before any new update.
#[derive(Debug)]
pub struct FinalizationPending(pub(crate) anyhow::Error);
impl std::fmt::Display for FinalizationPending {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "repository finalization pending: {:#}", self.0)
    }
}
impl std::error::Error for FinalizationPending {}

/// Whether a name in a platform directory is a package file or a package's
/// detached signature -- the only things a sweep ever considers.
fn is_package_file(name: &str) -> bool {
    !name.starts_with('.') && name.contains(".pkg.tar.")
}

/// What a sweep deleted, and what it found retired and left for later.
struct Swept {
    /// When each retired file still on disk was first found unlisted.
    still_retired: HashMap<PathBuf, Instant>,
    /// The names of the files it deleted.
    deleted: Vec<String>,
}

/// [`Repository::sweep`], given what is known of when files were retired.
fn sweep_retired(
    root: &Path,
    mut known: HashMap<PathBuf, Instant>,
    now: Instant,
    grace: Duration,
    log: &ActivityLog,
) -> Swept {
    let mut swept = Swept {
        still_retired: HashMap::new(),
        deleted: Vec::new(),
    };
    let Ok(platforms) = std::fs::read_dir(root) else {
        return swept;
    };
    for platform in platforms.flatten() {
        let dir = platform.path();
        let hidden = platform.file_name().to_string_lossy().starts_with('.');
        if hidden || !dir.is_dir() {
            continue;
        }
        let listed = match pacman_repo_utils::listed_filenames(&dir.join(DB_ARCHIVE)) {
            Ok(listed) => listed,
            Err(e) => {
                warn!(
                    "not sweeping {}: its repo.db cannot be read ({e})",
                    dir.display()
                );
                continue;
            }
        };
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !is_package_file(&name) {
                continue;
            }
            let package = name.strip_suffix(".sig").unwrap_or(&name);
            if listed.contains(package) {
                continue;
            }
            let path = entry.path();
            let since = known.remove(&path).unwrap_or(now);
            if now.saturating_duration_since(since) >= grace {
                match std::fs::remove_file(&path) {
                    Ok(()) => swept.deleted.push(name),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        log.emit(Event::RepoFileFailed {
                            path: path.display().to_string(),
                            action: FileAction::Remove,
                            error: error.to_string(),
                        });
                        swept.still_retired.insert(path, since);
                    }
                }
            } else {
                swept.still_retired.insert(path, since);
            }
        }
    }
    swept
}

/// A second name for `source` at `destination`, or a copy of it where the
/// filesystem has no hard links (SMB, some FUSE mounts): slower and twice the
/// space for a while, but publishing still works there.
fn link_or_copy(source: &Path, destination: &Path) -> std::io::Result<()> {
    match std::fs::hard_link(source, destination) {
        Ok(()) => Ok(()),
        Err(error) => {
            warn!(
                "could not hard-link {} ({error}); copying it instead",
                source.display()
            );
            std::fs::copy(source, destination).map(|_| ())
        }
    }
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    async fn test_db() -> sea_orm::DatabaseConnection {
        use sea_orm_migration::MigratorTrait;
        let db = sea_orm::Database::connect("sqlite::memory:").await.unwrap();
        aurcache_db::migration::Migrator::up(&db, None)
            .await
            .unwrap();
        db
    }

    /// The archive names are spelled out as constants, so they are checked
    /// against the name they derive from.
    #[test]
    fn the_archives_carry_the_repository_name() {
        assert_eq!(super::DB_ARCHIVE, format!("{}.db.tar.gz", super::REPO_NAME));
        assert_eq!(
            super::FILES_ARCHIVE,
            format!("{}.files.tar.gz", super::REPO_NAME)
        );
    }

    use super::*;
    use sea_orm::Statement;
    use std::io::Read;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A minimal valid `.pkg.tar.zst`: a zstd-compressed tar holding a
    /// `.PKGINFO` with a pkgname and pkgver, which is all a database entry needs.
    pub(crate) fn fake_pkg_zst(pkgname: &str, pkgver: &str) -> Vec<u8> {
        let pkginfo = format!("pkgname = {pkgname}\npkgver = {pkgver}\n");
        let mut tar_bytes = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_bytes);
            let mut header = tar::Header::new_gnu();
            header.set_path(".PKGINFO").unwrap();
            header.set_size(pkginfo.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append(&header, pkginfo.as_bytes()).unwrap();
            builder.finish().unwrap();
        }
        zstd::stream::encode_all(tar_bytes.as_slice(), 0).unwrap()
    }

    fn filename(pkgname: &str, pkgver: &str) -> String {
        format!("{pkgname}-{pkgver}-x86_64.pkg.tar.zst")
    }

    /// Stage a fake package for build `build_id` and describe it.
    async fn stage(
        repo: &Repository,
        build_id: i32,
        pkgname: &str,
        pkgver: &str,
    ) -> (PathBuf, PackageEntry) {
        let dir = repo.staging_dir(build_id);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(filename(pkgname, pkgver));
        std::fs::write(&path, fake_pkg_zst(pkgname, pkgver)).unwrap();
        let entry = repo.describe(path.clone()).await.unwrap();
        (path, entry)
    }

    /// The entry directories `repo.db` lists for x86_64.
    fn listed(repo: &Repository) -> Vec<String> {
        let path = repo.root().join("x86_64").join(DB_ARCHIVE);
        let mut data = Vec::new();
        std::fs::File::open(path)
            .unwrap()
            .read_to_end(&mut data)
            .unwrap();
        let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(data.as_slice()));
        let mut dirs: Vec<String> = archive
            .entries()
            .unwrap()
            .flatten()
            .filter(|e| e.header().entry_type().is_dir())
            .map(|e| e.path().unwrap().display().to_string())
            .collect();
        dirs.sort();
        dirs
    }

    fn next_files_left(repo: &Repository) -> Vec<String> {
        std::fs::read_dir(repo.root().join("x86_64"))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".next"))
            .collect()
    }

    /// The x86_64 file `filename`, as `update` would have read it.
    fn published(update: &Update<'_>, filename: &str) -> PublishedFile {
        update.published(files::Model {
            filename: filename.to_string(),
            id: 0,
            platform: Platform::X86_64,
            package_id: 0,
            size: None,
        })
    }

    /// A file read by one update cannot be retired by another: by then it may
    /// belong to someone else.
    #[tokio::test]
    async fn a_file_read_by_another_update_cannot_be_retired() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::new(tmp.path());
        let db = test_db().await;
        let stale = {
            let update = repo.begin(&db).await.unwrap();
            published(&update, &filename("foo", "1.0-1"))
        };

        let mut update = repo.begin(&db).await.unwrap();
        assert!(update.retire(&stale).is_err());
        let fresh = published(&update, &filename("foo", "1.0-1"));
        assert!(update.retire(&fresh).is_ok());
    }

    /// Commit a change that the database accepts.
    async fn apply(
        repo: &Repository,
        add: Vec<(PathBuf, PackageEntry)>,
        remove: &[String],
    ) -> anyhow::Result<()> {
        let db = test_db().await;
        let mut update = repo.begin(&db).await.unwrap();
        for (staged, entry) in add {
            update.add(Platform::X86_64, staged, entry);
        }
        for filename in remove {
            let file = published(&update, filename);
            update.retire(&file).unwrap();
        }
        update.commit(&db, |_| Box::pin(async { Ok(()) })).await
    }

    #[tokio::test]
    async fn an_update_publishes_its_additions_and_retirements() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::new(tmp.path());
        let (staged, entry) = stage(&repo, 1, "foo", "1.0-1").await;
        apply(&repo, vec![(staged.clone(), entry)], &[])
            .await
            .unwrap();
        assert_eq!(listed(&repo), ["foo-1.0-1"]);
        assert!(platform_file(&repo, "foo", "1.0-1").exists());
        assert!(
            staged.exists(),
            "uploads survive until publication is acknowledged"
        );

        let (staged, entry) = stage(&repo, 2, "foo", "1.1-1").await;
        apply(&repo, vec![(staged, entry)], &[filename("foo", "1.0-1")])
            .await
            .unwrap();
        assert_eq!(listed(&repo), ["foo-1.1-1"]);
        assert!(
            platform_file(&repo, "foo", "1.0-1").exists(),
            "a retired file stays downloadable until the sweep"
        );
        assert!(next_files_left(&repo).is_empty());
    }

    fn platform_file(repo: &Repository, pkgname: &str, pkgver: &str) -> PathBuf {
        repo.root().join("x86_64").join(filename(pkgname, pkgver))
    }

    /// Publish foo 1.0 with a detached signature, then 1.1 over it: 1.0 and
    /// its signature are retired.
    async fn retire_one(repo: &Repository) {
        let (staged, entry) = stage(repo, 1, "foo", "1.0-1").await;
        apply(repo, vec![(staged, entry)], &[]).await.unwrap();
        std::fs::write(
            with_suffix(&platform_file(repo, "foo", "1.0-1"), ".sig"),
            b"sig",
        )
        .unwrap();
        let (staged, entry) = stage(repo, 2, "foo", "1.1-1").await;
        apply(repo, vec![(staged, entry)], &[filename("foo", "1.0-1")])
            .await
            .unwrap();
    }

    /// A retired file, and its signature, go once unlisted for the grace
    /// period -- not before, and the published version is never touched.
    #[tokio::test]
    async fn the_sweep_deletes_retired_files_once_their_grace_is_over() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::new(tmp.path());
        retire_one(&repo).await;
        let old = platform_file(&repo, "foo", "1.0-1");
        let grace = Duration::from_secs(3600);

        assert_eq!(repo.sweep(&test_db().await, grace).await.unwrap(), 0);
        assert!(old.exists() && with_suffix(&old, ".sig").exists());

        let later = Instant::now() + grace;
        assert_eq!(
            repo.sweep_at(&test_db().await, later, grace).await.unwrap(),
            2
        );
        assert!(!old.exists());
        assert!(!with_suffix(&old, ".sig").exists());
        assert!(platform_file(&repo, "foo", "1.1-1").exists());
        assert_eq!(listed(&repo), ["foo-1.1-1"]);
    }

    /// A restart forgets when files were retired. The first sweep after it
    /// starts their grace then, so a restart can only keep a file longer.
    #[tokio::test]
    async fn after_a_restart_a_retired_file_gets_its_grace_again() {
        let tmp = tempfile::tempdir().unwrap();
        let grace = Duration::from_secs(3600);
        retire_one(&Repository::new(tmp.path())).await;

        let restarted = Repository::new(tmp.path());
        let old = platform_file(&restarted, "foo", "1.0-1");
        // Long after the retirement, but the first this process has seen of it.
        assert_eq!(restarted.sweep(&test_db().await, grace).await.unwrap(), 0);
        assert!(old.exists());

        let later = Instant::now() + grace;
        assert_eq!(
            restarted
                .sweep_at(&test_db().await, later, grace)
                .await
                .unwrap(),
            2
        );
        assert!(!old.exists());
    }

    #[tokio::test]
    async fn failed_removal_is_retried_without_another_grace_period() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::new(tmp.path());
        retire_one(&repo).await;
        let old = platform_file(&repo, "foo", "1.0-1");
        let grace = Duration::from_secs(3600);
        let later = Instant::now() + grace;

        // A directory makes remove_file fail even when tests run as root.
        std::fs::remove_file(&old).unwrap();
        std::fs::create_dir(&old).unwrap();
        let first = sweep_retired(
            repo.root(),
            repo.retired_times().clone(),
            later,
            grace,
            repo.log(),
        );
        assert!(first.still_retired.contains_key(&old));

        std::fs::remove_dir(&old).unwrap();
        std::fs::write(&old, b"retired package").unwrap();
        let retried = sweep_retired(
            repo.root(),
            first.still_retired,
            later + Duration::from_secs(1),
            grace,
            repo.log(),
        );
        assert_eq!(retried.deleted, [filename("foo", "1.0-1")]);
        assert!(!old.exists());
    }

    /// A file published again under the same name before the sweep is listed
    /// once more, and stays.
    #[tokio::test]
    async fn a_file_listed_again_is_not_swept() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::new(tmp.path());
        retire_one(&repo).await;
        let (staged, entry) = stage(&repo, 3, "foo", "1.0-1").await;
        apply(&repo, vec![(staged, entry)], &[]).await.unwrap();

        let later = Instant::now() + Duration::from_secs(7200);
        repo.sweep_at(&test_db().await, later, Duration::from_secs(3600))
            .await
            .unwrap();

        assert!(platform_file(&repo, "foo", "1.0-1").exists());
        assert_eq!(listed(&repo), ["foo-1.0-1", "foo-1.1-1"]);
    }

    /// The sweep only ever deletes package files a readable `repo.db` does not
    /// list: not the databases or anything else beside them, and nothing at
    /// all in a directory whose `repo.db` is missing.
    #[tokio::test]
    async fn the_sweep_leaves_everything_but_unlisted_packages_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::new(tmp.path());
        retire_one(&repo).await;
        let x86 = repo.root().join("x86_64");
        std::fs::write(x86.join("mirrorlist"), b"Server = x").unwrap();
        let orphan_dir = repo.root().join("aarch64");
        std::fs::create_dir_all(&orphan_dir).unwrap();
        std::fs::write(orphan_dir.join("bar-1.0-1-aarch64.pkg.tar.zst"), b"x").unwrap();

        let later = Instant::now() + Duration::from_secs(7200);
        assert_eq!(
            repo.sweep_at(&test_db().await, later, Duration::from_secs(3600))
                .await
                .unwrap(),
            2,
            "only foo 1.0 and its signature"
        );

        assert!(x86.join(DB_ARCHIVE).exists());
        assert!(x86.join(FILES_ARCHIVE).exists());
        assert!(x86.join("mirrorlist").exists());
        assert!(orphan_dir.join("bar-1.0-1-aarch64.pkg.tar.zst").exists());
    }

    /// A commit that fails for good leaves the repository exactly as it was:
    /// the databases untouched, the upload still staged, nothing left beside.
    #[tokio::test]
    async fn a_failed_commit_changes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::new(tmp.path());
        let db = test_db().await;
        let (staged, entry) = stage(&repo, 1, "foo", "1.0-1").await;
        apply(&repo, vec![(staged, entry)], &[]).await.unwrap();
        let db_before = std::fs::read(repo.root().join("x86_64").join(DB_ARCHIVE)).unwrap();

        let (staged, entry) = stage(&repo, 2, "bar", "1.0-1").await;
        let attempts = AtomicU32::new(0);
        let mut update = repo.begin(&db).await.unwrap();
        update.add(Platform::X86_64, staged.clone(), entry);
        let foo = published(&update, &filename("foo", "1.0-1"));
        update.retire(&foo).unwrap();
        let failed: anyhow::Result<()> = update
            .commit(&db, |_| {
                attempts.fetch_add(1, Ordering::SeqCst);
                Box::pin(async { anyhow::bail!("the database said no") })
            })
            .await;

        assert!(failed.is_err());
        assert_eq!(attempts.load(Ordering::SeqCst), COMMIT_ATTEMPTS);
        assert_eq!(
            std::fs::read(repo.root().join("x86_64").join(DB_ARCHIVE)).unwrap(),
            db_before
        );
        assert!(staged.exists(), "the upload must still be staged");
        assert!(
            repo.root()
                .join("x86_64")
                .join(filename("foo", "1.0-1"))
                .exists()
        );
        assert!(
            !repo
                .root()
                .join("x86_64")
                .join(filename("bar", "1.0-1"))
                .exists()
        );
        assert!(next_files_left(&repo).is_empty());
    }

    /// A commit that fails once is tried again, and the update still lands.
    #[tokio::test]
    async fn a_commit_that_fails_once_is_retried() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::new(tmp.path());
        let db = test_db().await;
        let (staged, entry) = stage(&repo, 1, "foo", "1.0-1").await;
        let commits = AtomicU32::new(0);

        let mut update = repo.begin(&db).await.unwrap();
        update.add(Platform::X86_64, staged, entry);
        update
            .commit(&db, |_| {
                let busy = commits.fetch_add(1, Ordering::SeqCst) == 0;
                Box::pin(async move {
                    if busy {
                        anyhow::bail!("busy");
                    }
                    Ok(())
                })
            })
            .await
            .unwrap();

        assert_eq!(commits.load(Ordering::SeqCst), 2);
        assert_eq!(listed(&repo), ["foo-1.0-1"]);
    }

    /// An update dropped without committing changes nothing, and lets the next
    /// one in.
    #[tokio::test]
    async fn an_update_dropped_uncommitted_changes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::new(tmp.path());
        let db = test_db().await;
        let (staged, entry) = stage(&repo, 1, "foo", "1.0-1").await;
        {
            let mut update = repo.begin(&db).await.unwrap();
            update.add(Platform::X86_64, staged.clone(), entry.clone());
        }
        assert!(staged.exists());
        assert!(!repo.root().join("x86_64").exists());

        apply(&repo, vec![(staged, entry)], &[]).await.unwrap();
        assert_eq!(listed(&repo), ["foo-1.0-1"]);
    }

    /// A rebuild at the same version removes and adds the same filename: the
    /// new file must be what is left.
    #[tokio::test]
    async fn replacing_a_file_with_one_of_the_same_name_keeps_the_new_one() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::new(tmp.path());
        for build_id in [1, 2] {
            let (staged, entry) = stage(&repo, build_id, "foo", "1.0-1").await;
            apply(&repo, vec![(staged, entry)], &[filename("foo", "1.0-1")])
                .await
                .unwrap();
        }
        assert!(
            repo.root()
                .join("x86_64")
                .join(filename("foo", "1.0-1"))
                .exists()
        );
        assert_eq!(listed(&repo), ["foo-1.0-1"]);
    }

    /// Two updates started together on one platform both land: the second
    /// starts from the databases the first wrote, not from the ones before it.
    #[tokio::test]
    async fn concurrent_updates_do_not_lose_each_other() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Arc::new(Repository::new(tmp.path()));
        let db = test_db().await;
        let (foo_staged, foo) = stage(&repo, 1, "foo", "1.0-1").await;
        let (bar_staged, bar) = stage(&repo, 2, "bar", "1.0-1").await;

        let slow = {
            let repo = Arc::clone(&repo);
            let db = db.clone();
            async move {
                let mut update = repo.begin(&db).await.unwrap();
                update.add(Platform::X86_64, foo_staged, foo);
                // Long enough for the other update to be waiting on it.
                update
                    .commit(&db, |_| {
                        Box::pin(async {
                            tokio::time::sleep(Duration::from_millis(200)).await;
                            Ok(())
                        })
                    })
                    .await
            }
        };
        let fast = {
            let repo = Arc::clone(&repo);
            async move {
                tokio::time::sleep(Duration::from_millis(50)).await;
                apply(&repo, vec![(bar_staged, bar)], &[]).await
            }
        };
        let (a, b) = tokio::join!(slow, fast);
        a.unwrap();
        b.unwrap();

        assert_eq!(listed(&repo), ["bar-1.0-1", "foo-1.0-1"]);
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn cancellation_keeps_the_lock_until_blocking_preparation_ends() {
        use std::io::Write;

        let tmp = tempfile::tempdir().unwrap();
        let repo = Arc::new(Repository::new(tmp.path()));
        let db = test_db().await;
        let (staged, entry) = stage(&repo, 1, "foo", "1.0-1").await;
        apply(&repo, vec![(staged, entry)], &[]).await.unwrap();
        let archive = repo.root().join("x86_64").join(DB_ARCHIVE);
        let original = std::fs::read(&archive).unwrap();
        std::fs::remove_file(&archive).unwrap();
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&archive)
                .status()
                .unwrap()
                .success()
        );

        // Reading the current database now blocks on this FIFO, so cancellation
        // occurs inside the real filesystem preparation, not a test-only hook.
        let (started, reading) = tokio::sync::oneshot::channel();
        let (release, released) = std::sync::mpsc::channel();
        let fifo = archive.clone();
        let contents = original.clone();
        let writer = std::thread::spawn(move || {
            let mut file = std::fs::OpenOptions::new().write(true).open(fifo).unwrap();
            let _ = started.send(());
            let _ = released.recv_timeout(Duration::from_secs(5));
            file.write_all(&contents).unwrap();
        });
        let (staged, entry) = stage(&repo, 2, "bar", "1.0-1").await;
        let publishing = {
            let repo = Arc::clone(&repo);
            let db = db.clone();
            tokio::spawn(async move {
                let mut update = repo.begin(&db).await.unwrap();
                update.add(Platform::X86_64, staged, entry);
                update.commit(&db, |_| Box::pin(async { Ok(()) })).await
            })
        };
        tokio::time::timeout(Duration::from_secs(5), reading)
            .await
            .unwrap()
            .unwrap();
        publishing.abort();
        assert!(publishing.await.unwrap_err().is_cancelled());
        assert!(
            tokio::time::timeout(Duration::from_millis(100), repo.begin(&db))
                .await
                .is_err()
        );
        release.send(()).unwrap();
        writer.join().unwrap();
        let update = tokio::time::timeout(Duration::from_secs(5), repo.begin(&db))
            .await
            .unwrap()
            .unwrap();
        drop(update);
        std::fs::remove_file(&archive).unwrap();
        std::fs::write(&archive, original).unwrap();
        assert_eq!(listed(&repo), ["foo-1.0-1"]);
        assert!(!platform_file(&repo, "bar", "1.0-1").exists());
    }

    #[tokio::test]
    async fn each_failed_move_is_durable_and_replayable_after_restart() {
        for blocked_name in [
            filename("foo", "1.0-1"),
            DB_ARCHIVE.to_string(),
            FILES_ARCHIVE.to_string(),
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let db = test_db().await;
            let repo = Repository::new(tmp.path());
            let (staged, entry) = stage(&repo, 1, "foo", "1.0-1").await;
            let blocked = tmp.path().join("x86_64").join(&blocked_name);
            let mut update = repo.begin(&db).await.unwrap();
            update.add(Platform::X86_64, staged.clone(), entry);
            let error = update
                .commit(&db, |_| {
                    let blocked = blocked.clone();
                    Box::pin(async move {
                        if blocked.exists() {
                            std::fs::remove_file(&blocked)?;
                        }
                        std::fs::create_dir(&blocked)?;
                        Ok(())
                    })
                })
                .await
                .unwrap_err();
            assert!(error.downcast_ref::<FinalizationPending>().is_some());
            assert!(staged.exists());
            let restarted = Repository::new(tmp.path());
            assert!(restarted.begin(&db).await.is_err());
            assert!(restarted.sweep(&db, Duration::ZERO).await.is_err());
            std::fs::remove_dir(&blocked).unwrap();
            restarted.recover(&db).await.unwrap();
            restarted.recover(&db).await.unwrap();
            assert_eq!(listed(&restarted), ["foo-1.0-1"]);
            assert!(platform_file(&restarted, "foo", "1.0-1").is_file());
            assert!(tmp.path().join("x86_64").join(FILES_ARCHIVE).is_file());
            assert!(
                db.query_one_raw(Statement::from_string(
                    db.get_database_backend(),
                    "SELECT id FROM repository_updates"
                ))
                .await
                .unwrap()
                .is_none()
            );
        }
    }

    /// A committed plan whose staged file was lost before it could be moved
    /// cannot ever finish; it must not block the repository for good.
    #[tokio::test]
    async fn a_plan_whose_staged_file_is_gone_does_not_block_the_repository() {
        let tmp = tempfile::tempdir().unwrap();
        let db = test_db().await;
        let repo = Repository::new(tmp.path());
        let (staged, entry) = stage(&repo, 1, "foo", "1.0-1").await;
        let blocked = platform_file(&repo, "foo", "1.0-1");
        let mut update = repo.begin(&db).await.unwrap();
        update.add(Platform::X86_64, staged, entry);
        let error = update
            .commit(&db, |_| {
                let blocked = blocked.clone();
                Box::pin(async move {
                    std::fs::create_dir(&blocked)?;
                    Ok(())
                })
            })
            .await
            .unwrap_err();
        assert!(error.downcast_ref::<FinalizationPending>().is_some());

        // The staging area is cleared by hand while the plan waits.
        std::fs::remove_dir(&blocked).unwrap();
        std::fs::remove_dir_all(repo.staging_root()).unwrap();

        let restarted = Repository::new(tmp.path());
        restarted.recover(&db).await.unwrap();
        assert!(restarted.begin(&db).await.is_ok());
        assert!(plan_row::read(&db).await.unwrap().is_none());
    }
}
