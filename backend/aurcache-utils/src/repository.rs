//! The pacman repository this server publishes: the package files, the
//! `repo.db`/`repo.files` databases indexing them, the `files` rows recording
//! who publishes what, and the staging area uploads wait in. Every change to it
//! goes through [`Repository::begin`].
//!
//! A repository given an [`InstanceKey`] signs: each package as it is
//! described, before the lock, and each database as an update writes it, so
//! that a signature always lands with what it signs (see
//! [`Repository::with_key`]).

use aurcache_activitylog::activity_utils::ActivityLog;
use aurcache_activitylog::events::{Event, FileAction};
use aurcache_common::api::repo::SigningInfo;
use aurcache_common::repo::{DEFAULT_REPO_NAME, validate_name};
use aurcache_db::files;
use aurcache_db::prelude::Files;
use aurcache_signing::InstanceKey;
use pacman_mirrors::platforms::Platform;
use pacman_repo_utils::{ListedPackage, PackageEntry};
use sea_orm::{
    ColumnTrait, ConnectionTrait, DatabaseConnection, DatabaseTransaction, EntityTrait,
    QueryFilter, TransactionTrait,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tracing::warn;

pub use aurcache_common::fs::REPO_ROOT;

/// Where uploads wait to be published, under the repository root.
///
/// Dot-named, and the repository's file server refuses dot segments, so an
/// upload is never downloadable before it is published.
const STAGING_DIR: &str = ".staging";

/// Where a backfill stages the files it signs, under the staging directory:
/// not a number, so never taken for a build's.
const RESIGN_DIR: &str = "resign";

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
    name: String,
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
    /// The key the repository signs with, when it signs.
    signing: Option<Signing>,
}

/// How the repository signs.
struct Signing {
    key: Arc<InstanceKey>,
    /// Whether everything the repository lists is known to carry the key's
    /// signature: false until a backfill finds nothing left to sign
    /// ([`Repository::mark_signed`]).
    complete: AtomicBool,
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

/// What one update adds, signs again and retires.
#[derive(Default)]
struct Changes {
    add: Vec<Addition>,
    /// Published files whose entries are replaced, and whose signatures move
    /// in, while the files themselves stay where they are.
    resign: Vec<Addition>,
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

    fn resign(&mut self, platform: Platform, staged: PathBuf, entry: PackageEntry) {
        self.resign.push(Addition {
            platform,
            staged,
            entry,
        });
    }

    /// Every addition and re-signing, whose staged files must be under the
    /// repository root.
    fn staged(&self) -> impl Iterator<Item = &Addition> {
        self.add.iter().chain(&self.resign)
    }

    fn platforms(&self) -> Vec<Platform> {
        let mut platforms: Vec<Platform> = self
            .staged()
            .map(|a| a.platform)
            .chain(self.retire.iter().map(|r| r.platform))
            .collect();
        platforms.sort_by_key(Platform::as_str);
        platforms.dedup();
        platforms
    }
}

/// One change to the repository, holding its lock (the `lock` field). See
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
    /// dropped. Shared with the blocking tasks the commit runs, so that the
    /// lock outlives one whose awaiting future is cancelled.
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
    lock: Lock,
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

    fn published(&self, row: files::Model) -> PublishedFile {
        PublishedFile {
            update: self.id,
            row,
        }
    }

    /// Publish the package file `entry` describes, moving it from `staged`,
    /// and its detached signature `<staged>.sig` with it. Without one, a
    /// signature a file of the same name left behind goes.
    pub fn add(&mut self, platform: Platform, staged: PathBuf, entry: PackageEntry) {
        self.changes.add(platform, staged, entry);
    }

    /// Replace the entry of a file already published on `platform` with
    /// `entry`, moving in the detached signature beside `staged` -- a copy of
    /// the published file, which itself stays where it is.
    ///
    /// For signing what was published unsigned, or by another key. The caller
    /// checks, through [`Update::listed`], that the file is still the one the
    /// copy was made from.
    pub fn resign(&mut self, platform: Platform, staged: PathBuf, entry: PackageEntry) {
        self.changes.resign(platform, staged, entry);
    }

    /// What `platform`'s `repo.db` lists, read under the lock: nothing can
    /// change it before this update commits.
    pub async fn listed(&self, platform: Platform) -> anyhow::Result<Vec<ListedPackage>> {
        self.repo.listed(platform).await
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
        let changes = Arc::new(std::mem::take(&mut self.changes));
        if let Some(addition) = changes.staged().find(|a| !a.staged.starts_with(&root)) {
            anyhow::bail!(
                "{} is not staged under {}",
                addition.staged.display(),
                root.display()
            );
        }
        let plan = {
            let root = root.clone();
            let changes = Arc::clone(&changes);
            let name = self.repo.name.clone();
            let lock = Arc::clone(&self.lock);
            let key = self.repo.key_handle();
            tokio::task::spawn_blocking(move || {
                let _lock = lock;
                Finalization::stage(&root, &name, &changes, key.as_deref())
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
            .finalize(db, plan, Arc::clone(&self.lock))
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

/// The repository lock, held: see [`Update`]'s field of that name. Shared, so
/// a blocking task can hold it too.
type Lock = Arc<tokio::sync::OwnedMutexGuard<()>>;

impl Repository {
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            name: DEFAULT_REPO_NAME.to_string(),
            lock: Arc::new(tokio::sync::Mutex::new(())),
            updates: AtomicU64::new(0),
            retired: std::sync::Mutex::new(HashMap::new()),
            log: ActivityLog::discarding(),
            signing: None,
        }
    }

    /// Set the name before initializing or using the repository.
    pub fn with_name(mut self, name: String) -> anyhow::Result<Self> {
        validate_name(&name).map_err(anyhow::Error::msg)?;
        self.name = name;
        Ok(self)
    }

    /// The pacman section and database filename prefix.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Sign with `key`: every package as [`Self::describe`] reads it, and the
    /// databases every update writes, each signature moved in right after
    /// what it signs.
    ///
    /// What was published before is signed by a backfill
    /// ([`crate::signing::backfill`]); until one has found nothing left,
    /// [`Self::signing_info`] says so.
    #[must_use]
    pub fn with_key(mut self, key: InstanceKey) -> Self {
        self.signing = Some(Signing {
            key: Arc::new(key),
            complete: AtomicBool::new(false),
        });
        self
    }

    /// The key the repository signs with, if it signs.
    #[must_use]
    pub fn key(&self) -> Option<&InstanceKey> {
        self.signing.as_ref().map(|signing| signing.key.as_ref())
    }

    /// The key, for a blocking task to take along.
    fn key_handle(&self) -> Option<Arc<InstanceKey>> {
        self.signing
            .as_ref()
            .map(|signing| Arc::clone(&signing.key))
    }

    /// What a client needs to know about the repository's signatures: `None`
    /// when it does not sign.
    #[must_use]
    pub fn signing_info(&self) -> Option<SigningInfo> {
        self.signing.as_ref().map(|signing| SigningInfo {
            fingerprint: signing.key.fingerprint().to_string(),
            backfilling: !signing.complete.load(Ordering::Acquire),
        })
    }

    /// Record that everything the repository lists carries the key's
    /// signature, which a backfill that found nothing left to sign knows.
    /// Everything published from then on is signed as it is published.
    pub fn mark_signed(&self) {
        if let Some(signing) = &self.signing {
            signing.complete.store(true, Ordering::Release);
        }
    }

    /// Record that what the repository lists may not all carry the key's
    /// signature any more -- the component key that signs changed, and what
    /// the old one signed waits to be signed again -- until a backfill finds
    /// nothing left.
    pub fn mark_unsigned(&self) {
        if let Some(signing) = &self.signing {
            signing.complete.store(false, Ordering::Release);
        }
    }

    /// Create the databases of every platform that has none yet, and the
    /// links pacman fetches their signatures through -- or, for a repository
    /// that does not sign, remove any signatures of its databases left from
    /// when it did ([`Self::init_signatures`]).
    pub fn init(&self) -> anyhow::Result<()> {
        // Renaming an existing repository requires renaming its archives too.
        // Creating empty ones here would hide its packages and let the sweep
        // retire them. Check every platform before creating anything.
        for platform in Platform::ALL {
            let dir = Self::platform_dir(&self.root, platform);
            if dir.join(format!("{}.db.tar.gz", self.name)).exists() {
                continue;
            }
            if let Ok(entries) = std::fs::read_dir(&dir) {
                for entry in entries {
                    let entry = entry?;
                    let file_name = entry.file_name();
                    let file_name = file_name.to_string_lossy();
                    // A database's signature is part of it, renamed with it.
                    let database = file_name.strip_suffix(".sig").unwrap_or(&file_name);
                    if database.ends_with(".db.tar.gz") {
                        anyhow::bail!(
                            "{} contains a differently named repository; rename its database archives, their signatures and symlinks to '{}' before changing AURCACHE_REPO_NAME",
                            dir.display(),
                            self.name
                        );
                    }
                }
            }
        }
        for platform in Platform::ALL {
            let dir = Self::platform_dir(&self.root, platform);
            pacman_repo_utils::repo_init::init_repo(&dir, &self.name)?;
            self.init_signatures(&dir)?;
        }
        Ok(())
    }

    /// Make `dir`'s database signatures what this repository serves: the
    /// links pacman fetches them through (`<name>.db.sig`, as `repo-add
    /// --sign` makes), for a repository that signs; for one that does not,
    /// none at all -- its next update rewrites the databases unsigned, and
    /// pacman refuses a signature that does not match even where signatures
    /// are optional.
    fn init_signatures(&self, dir: &Path) -> anyhow::Result<()> {
        if self.signing.is_some() {
            pacman_repo_utils::repo_init::link_signatures(dir, &self.name)
        } else {
            pacman_repo_utils::repo_init::remove_signatures(dir, &self.name)
        }
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

    /// Remove a build's [`Repository::staging_dir`], once nothing in it is
    /// going to be published.
    ///
    /// Best-effort: the directory is often gone already, and one that cannot
    /// be removed holds nothing anything reads again.
    pub async fn discard_staging(&self, build_id: i32) {
        let _ = tokio::fs::remove_dir_all(self.staging_dir(build_id)).await;
    }

    /// The directory every build's [`Repository::staging_dir`] is in.
    #[must_use]
    pub fn staging_root(&self) -> PathBuf {
        self.root.join(STAGING_DIR)
    }

    /// Read a staged package into its database entry, signing it first when
    /// the repository signs. The slow part of publishing -- the whole archive
    /// is read -- and it changes nothing published, so it needs no lock and
    /// belongs before [`Repository::begin`].
    ///
    /// The signature is written beside the staged file, replacing whatever
    /// was there (a worker's upload of that name included), so the entry
    /// carries it as `%PGPSIG%` and the update moves it in with the package.
    pub async fn describe(&self, staged: PathBuf) -> anyhow::Result<PackageEntry> {
        let key = self.key_handle();
        tokio::task::spawn_blocking(move || {
            if let Some(key) = key {
                sign_staged(&key, &staged)?;
            }
            pacman_repo_utils::describe_package(&staged)
        })
        .await?
    }

    /// What `platform`'s `<name>.db` lists now, read without the lock: a
    /// change may land before anything is done with it. An update reads the
    /// same under the lock ([`Update::listed`]).
    pub async fn listed(&self, platform: Platform) -> anyhow::Result<Vec<ListedPackage>> {
        let db = Self::platform_dir(&self.root, platform).join(format!("{}.db.tar.gz", self.name));
        tokio::task::spawn_blocking(move || {
            if db.exists() {
                pacman_repo_utils::listed_packages(&db)
            } else {
                Ok(Vec::new())
            }
        })
        .await?
    }

    /// The entries `platform`'s database lists whose file does not carry the
    /// key's signature over what it is now: unsigned, signed by another key
    /// or another of its subkeys, or with a `%PGPSIG%` that names the key but
    /// is not over the file. Each file the key signed is read whole to tell,
    /// on a blocking thread and without the lock: a change may land
    /// meanwhile, which [`Update::resign`]'s caller checks.
    pub async fn unsigned(&self, platform: Platform) -> anyhow::Result<Vec<ListedPackage>> {
        let Some(key) = self.key_handle() else {
            return Ok(Vec::new());
        };
        let listed = self.listed(platform).await?;
        let dir = Self::platform_dir(&self.root, platform);
        let unsigned = tokio::task::spawn_blocking(move || {
            listed
                .into_iter()
                .filter(|entry| {
                    !entry.signature.as_deref().is_some_and(|signature| {
                        key.issued(signature)
                            && key.verifies_file(signature, &dir.join(&entry.filename))
                    })
                })
                .collect()
        })
        .await?;
        Ok(unsigned)
    }

    /// Where a backfill stages copies of `platform`'s published files, to sign
    /// and describe them.
    #[must_use]
    pub fn resign_dir(&self, platform: Platform) -> PathBuf {
        self.staging_root().join(RESIGN_DIR).join(platform.as_str())
    }

    /// A second name in [`Self::resign_dir`] for the file `filename` published
    /// on `platform` -- a copy where the filesystem has no hard links -- for a
    /// backfill to sign and describe. Taken without the lock: whatever an
    /// update does to the published name afterwards, this one keeps the file
    /// it was, which is what [`Update::resign`]'s caller checks against.
    pub async fn stage_published(
        &self,
        platform: Platform,
        filename: &str,
    ) -> anyhow::Result<PathBuf> {
        if Path::new(filename).file_name() != Some(std::ffi::OsStr::new(filename)) {
            anyhow::bail!("{filename:?} is not a file name");
        }
        let published = Self::platform_dir(&self.root, platform).join(filename);
        let dir = self.resign_dir(platform);
        let staged = dir.join(filename);
        let result = staged.clone();
        tokio::task::spawn_blocking(move || {
            std::fs::create_dir_all(&dir)?;
            match std::fs::remove_file(&staged) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            link_or_copy(&published, &staged)
        })
        .await??;
        Ok(result)
    }

    /// Sign every platform's databases that do not carry a signature of the
    /// key over what they are now, in place.
    ///
    /// For databases no update has rewritten since signing began. A
    /// signature written here is for the databases exactly as they are,
    /// read under the lock, and replaces any other at once, so a client sees
    /// the old signature or the new one, each beside the databases it signs.
    /// Nothing is recorded for recovery: an interruption leaves what was
    /// there, and the next call does it again.
    pub async fn sign_databases(&self, db: &DatabaseConnection) -> anyhow::Result<()> {
        let Some(key) = self.key_handle() else {
            return Ok(());
        };
        let lock = self.lock().await;
        self.recover_locked(db, Arc::clone(&lock)).await?;
        let root = self.root.clone();
        let name = self.name.clone();
        tokio::task::spawn_blocking(move || {
            let _lock = lock;
            for platform in Platform::ALL {
                let dir = Self::platform_dir(&root, platform);
                for archive in ["db", "files"] {
                    sign_in_place(&key, &dir.join(format!("{name}.{archive}.tar.gz")))?;
                }
            }
            Ok(())
        })
        .await?
    }

    /// Start changing the repository.
    ///
    /// Takes the repository lock, which the returned update holds until it is
    /// committed or dropped. What the repository publishes is read through the
    /// update, so every decision about what to change is made with no other
    /// change able to interleave. Dropping it without committing changes
    /// nothing.
    pub async fn begin(&self, db: &DatabaseConnection) -> anyhow::Result<Update<'_>> {
        let lock = self.lock().await;
        self.recover_locked(db, Arc::clone(&lock)).await?;
        Ok(Update {
            repo: self,
            id: self.updates.fetch_add(1, Ordering::Relaxed),
            lock,
            changes: Changes::default(),
        })
    }

    /// Finish a previously committed update before accepting more repository work.
    pub async fn recover(&self, db: &DatabaseConnection) -> anyhow::Result<()> {
        let lock = self.lock().await;
        self.recover_locked(db, lock).await
    }

    /// Wait for the repository lock.
    async fn lock(&self) -> Lock {
        Arc::new(Arc::clone(&self.lock).lock_owned().await)
    }

    async fn recover_locked(&self, db: &DatabaseConnection, lock: Lock) -> anyhow::Result<()> {
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
        lock: Lock,
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
        let lock = self.lock().await;
        self.recover_locked(db, Arc::clone(&lock)).await?;
        let root = self.root.clone();
        let name = self.name.clone();
        let known = std::mem::take(&mut *self.retired_times());
        let log = self.log.clone();
        let task_lock = Arc::clone(&lock);
        let Swept {
            still_retired,
            deleted,
        } = tokio::task::spawn_blocking(move || {
            let _lock = task_lock;
            sweep_retired(&root, &name, known, now, grace, &log)
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

/// A durable filesystem plan, recorded in the same transaction as its SQL changes.
#[derive(Serialize, Deserialize)]
struct Finalization {
    moves: Vec<FileMove>,
    retired: Vec<PathBuf>,
    /// Signatures beside package files the update replaces with unsigned
    /// ones of the same name, removed once the moves are done. They sign
    /// what is no longer there, and retiring them would keep them: the sweep
    /// keeps a signature whose package is listed, and this one's still is.
    #[serde(default)]
    removed: Vec<PathBuf>,
}

#[derive(Serialize, Deserialize)]
struct FileMove {
    source: PathBuf,
    destination: PathBuf,
}

impl Finalization {
    /// Everything the update will move into place, made ready beside where it
    /// goes and synced to disk, before anything is committed: each package
    /// file and its signature linked out of staging, then each touched
    /// platform's new databases written beside the current ones. All of it is
    /// private -- staging and dot-named files are never served -- and all of it
    /// is removed again if any step fails.
    ///
    /// The packages are planned before the databases, so a replay moves them
    /// in first: a database never lists a file that is not there yet.
    ///
    /// With a `key`, each database is signed as it is written, and its
    /// signature is moved in right after it.
    fn stage(
        root: &Path,
        name: &str,
        changes: &Changes,
        key: Option<&InstanceKey>,
    ) -> anyhow::Result<Self> {
        let mut plan = Self {
            moves: Vec::new(),
            retired: Vec::new(),
            removed: Vec::new(),
        };
        if let Err(error) = plan.stage_all(root, name, changes, key) {
            plan.discard();
            return Err(error);
        }
        Ok(plan)
    }

    fn stage_all(
        &mut self,
        root: &Path,
        name: &str,
        changes: &Changes,
        key: Option<&InstanceKey>,
    ) -> anyhow::Result<()> {
        let mut added = HashSet::new();
        for addition in &changes.add {
            let filename = addition.entry.filename.as_str();
            let dir = Repository::platform_dir(root, addition.platform);
            added.insert((addition.platform, filename));
            let package = dir.join(filename);
            let signature = with_suffix(&package, ".sig");
            self.stage_file(&addition.staged, package)?;
            let staged_signature = with_suffix(&addition.staged, ".sig");
            if staged_signature.exists() {
                self.stage_file(&staged_signature, signature)?;
            } else if signature.exists() {
                self.removed.push(signature);
            }
        }
        for resigned in &changes.resign {
            let filename = resigned.entry.filename.as_str();
            let dir = Repository::platform_dir(root, resigned.platform);
            self.stage_file(
                &with_suffix(&resigned.staged, ".sig"),
                dir.join(format!("{filename}.sig")),
            )?;
        }
        for platform in changes.platforms() {
            self.stage_databases(root, name, changes, platform, key)
                .map_err(|e| e.context(format!("preparing the {platform} databases")))?;
        }
        for Retirement { platform, filename } in &changes.retire {
            if added.contains(&(*platform, filename.as_str())) {
                continue;
            }
            let path = Repository::platform_dir(root, *platform).join(filename);
            self.retired.push(with_suffix(&path, ".sig"));
            self.retired.push(path);
        }
        Ok(())
    }

    /// Plan moving `source` to `destination`, through a second name beside
    /// it. The upload itself stays where it is until publication (including
    /// dependent promotion) finishes, so recovery can safely retry a build
    /// still `PUBLISHING`.
    fn stage_file(&mut self, source: &Path, destination: PathBuf) -> anyhow::Result<()> {
        let pending = with_suffix(source, ".publishing");
        if pending.exists() {
            std::fs::remove_file(&pending)?;
        }
        link_or_copy(source, &pending)?;
        self.moves.push(FileMove {
            source: pending.clone(),
            destination,
        });
        sync(&pending)
    }

    /// Plan replacing `platform`'s databases with new ones, written beside
    /// them from their current contents and the changes, each followed by
    /// its signature when there is a `key`.
    ///
    /// A database and its signature are two files, and no rename replaces
    /// two at once, so the signature is moved in immediately after its
    /// database (see [`Finalization::finish`]). In between, a client can
    /// fetch the new database and the old signature, fail that one refresh,
    /// and succeed on the next: the same window `repo-add` leaves.
    fn stage_databases(
        &mut self,
        root: &Path,
        name: &str,
        changes: &Changes,
        platform: Platform,
        key: Option<&InstanceKey>,
    ) -> anyhow::Result<()> {
        let dir = Repository::platform_dir(root, platform);
        std::fs::create_dir_all(&dir)?;
        let db = dir.join(format!("{name}.db.tar.gz"));
        let db_next = dir.join(format!(".{name}.db.tar.gz.next"));
        let files = dir.join(format!("{name}.files.tar.gz"));
        let files_next = dir.join(format!(".{name}.files.tar.gz.next"));
        // Planned before they are written, so that a write failing halfway
        // is cleaned up with the rest.
        for (source, destination) in [(&db_next, &db), (&files_next, &files)] {
            self.moves.push(FileMove {
                source: source.clone(),
                destination: destination.clone(),
            });
            if key.is_some() {
                self.moves.push(FileMove {
                    source: signature_next(source),
                    destination: with_suffix(destination, ".sig"),
                });
            }
        }
        let remove: Vec<String> = changes
            .retire
            .iter()
            .filter(|r| r.platform == platform)
            .map(|r| r.filename.clone())
            .collect();
        let add: Vec<PackageEntry> = changes
            .staged()
            .filter(|a| a.platform == platform)
            .map(|a| a.entry.clone())
            .collect();
        pacman_repo_utils::write_updated_databases(
            &db,
            &files,
            &remove,
            &add,
            &db_next,
            &files_next,
        )?;
        for next in [&db_next, &files_next] {
            sync(next)?;
            if let Some(key) = key {
                let signature = signature_next(next);
                std::fs::write(&signature, key.sign_file(next)?)?;
                sync(&signature)?;
            }
        }
        Ok(())
    }

    /// Remove replacements that have never been committed to SQL.
    fn discard(&self) {
        for movement in &self.moves {
            let _ = std::fs::remove_file(&movement.source);
        }
    }

    /// Move everything into place, in the order planned, each move made
    /// durable before the next -- except that a file and its detached
    /// signature are renamed back to back and made durable together, to keep
    /// the moment a client could see one without the other as short as two
    /// renames.
    fn finish(self, log: &ActivityLog) -> anyhow::Result<Vec<PathBuf>> {
        let mut unsynced: Option<&Path> = None;
        for (index, movement) in self.moves.iter().enumerate() {
            let moved = match std::fs::rename(&movement.source, &movement.destination) {
                Ok(()) => true,
                // A restart replays the whole plan, including moves already done.
                Err(error)
                    if error.kind() == std::io::ErrorKind::NotFound
                        && !movement.source.exists()
                        && movement.destination.is_file() =>
                {
                    true
                }
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
                    false
                }
                Err(error) => {
                    log.emit(Event::RepoFileFailed {
                        action: FileAction::Move,
                        path: movement.source.display().to_string(),
                        error: error.to_string(),
                    });
                    return Err(error.into());
                }
            };
            if moved {
                unsynced = movement.destination.parent().or(unsynced);
            }
            let signature_follows = moved
                && self.moves.get(index + 1).is_some_and(|next| {
                    next.destination == with_suffix(&movement.destination, ".sig")
                });
            if !signature_follows && let Some(dir) = unsynced.take() {
                std::fs::File::open(dir)?.sync_all()?;
            }
        }
        for path in &self.removed {
            match std::fs::remove_file(path) {
                // A restart replays the whole plan, including removals done.
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    log.emit(Event::RepoFileFailed {
                        action: FileAction::Remove,
                        path: path.display().to_string(),
                        error: error.to_string(),
                    });
                    return Err(error.into());
                }
            }
            // Synced like a move, and on a replay too: until the directory
            // is, a power loss could bring the file back with no plan left
            // to remove it.
            if let Some(parent) = path.parent() {
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
    name: &str,
    mut known: HashMap<PathBuf, Instant>,
    now: Instant,
    grace: Duration,
    log: &ActivityLog,
) -> Swept {
    let database_names =
        ["db", "files", "db.tar.gz", "files.tar.gz"].map(|suffix| format!("{name}.{suffix}"));
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
        let listed =
            match pacman_repo_utils::listed_filenames(&dir.join(format!("{name}.db.tar.gz"))) {
                Ok(listed) => listed,
                Err(e) => {
                    warn!(
                        "not sweeping {}: its {name}.db cannot be read ({e})",
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
            // A valid repository name can itself contain `.pkg.tar.`.
            // Its databases (and signatures) are never retired packages.
            if database_names.iter().any(|database| database == package) || listed.contains(package)
            {
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

/// Flush `path`, and the directory entry naming it, to disk.
fn sync(path: &Path) -> anyhow::Result<()> {
    std::fs::File::open(path)?.sync_all()?;
    std::fs::File::open(path.parent().expect("a file in a directory"))?.sync_all()?;
    Ok(())
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// Where the signature of a database being written as `next` is written:
/// `.repo.db.tar.gz.next` is signed in `.repo.db.tar.gz.sig.next`.
fn signature_next(next: &Path) -> PathBuf {
    let name = next
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let database = name.strip_suffix(".next").unwrap_or(&name);
    next.with_file_name(format!("{database}.sig.next"))
}

/// Sign the staged package `staged` with `key`, writing the signature
/// beside it as `<staged>.sig` -- replacing whatever was there, a worker's
/// upload of that name included -- where [`pacman_repo_utils::describe_package`]
/// reads it into `%PGPSIG%` and an update moves it in with the package.
fn sign_staged(key: &InstanceKey, staged: &Path) -> anyhow::Result<()> {
    write_signature(&with_suffix(staged, ".sig"), &key.sign_file(staged)?)
}

/// Write `signature` at `path`, through a dot-named file beside it: what is
/// at `path` is always a whole signature, and a new file rather than one
/// rewritten in place, so that another name an earlier attempt gave the old
/// one never changes under it.
fn write_signature(path: &Path, signature: &[u8]) -> anyhow::Result<()> {
    let name = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("{} names no file", path.display()))?
        .to_string_lossy();
    let partial = path.with_file_name(format!(".{name}.partial"));
    std::fs::write(&partial, signature)?;
    sync(&partial)?;
    std::fs::rename(&partial, path)?;
    sync(path)
}

/// Sign `archive` with `key` beside it, unless the signature there already
/// is the key's over what the archive is now. An archive that is not there
/// has nothing to sign.
fn sign_in_place(key: &InstanceKey, archive: &Path) -> anyhow::Result<()> {
    let content = match std::fs::read(archive) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let path = with_suffix(archive, ".sig");
    let current = match std::fs::read(&path) {
        Ok(signature) => Some(signature),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    if current.is_some_and(|signature| key.verifies(&signature, &content)) {
        return Ok(());
    }
    write_signature(&path, &key.sign(content.as_slice())?)
}

#[cfg(test)]
mod tests {

    const DB_ARCHIVE: &str = "repo.db.tar.gz";
    const FILES_ARCHIVE: &str = "repo.files.tar.gz";

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
        let path = repo
            .root()
            .join("x86_64")
            .join(format!("{}.db.tar.gz", repo.name()));
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
        let db = aurcache_db::testing::memory_db().await;
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
        let db = aurcache_db::testing::memory_db().await;
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
    async fn a_custom_name_survives_initialization_publication_restart_and_sweep() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::new(tmp.path())
            .with_name("custom_name".into())
            .unwrap();
        repo.init().unwrap();
        for platform in Platform::ALL {
            let dir = Repository::platform_dir(tmp.path(), platform);
            for suffix in ["db", "files"] {
                assert_eq!(
                    std::fs::read_link(dir.join(format!("custom_name.{suffix}"))).unwrap(),
                    PathBuf::from(format!("custom_name.{suffix}.tar.gz"))
                );
            }
            assert!(!dir.join("repo.db").exists());
        }
        retire_one(&repo).await;
        let restarted = Repository::new(tmp.path())
            .with_name("custom_name".into())
            .unwrap();
        restarted.init().unwrap();
        assert_eq!(listed(&restarted), ["foo-1.1-1"]);
        let db = aurcache_db::testing::memory_db().await;
        restarted.sweep(&db, Duration::from_secs(60)).await.unwrap();
        assert_eq!(
            restarted
                .sweep_at(
                    &db,
                    Instant::now() + Duration::from_secs(120),
                    Duration::from_secs(60)
                )
                .await
                .unwrap(),
            2
        );
        assert!(platform_file(&restarted, "foo", "1.1-1").exists());
        assert!(!platform_file(&restarted, "foo", "1.0-1").exists());
        assert_eq!(listed(&restarted), ["foo-1.1-1"]);
    }

    #[tokio::test]
    async fn a_package_like_repository_name_does_not_let_the_sweep_delete_its_databases() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::new(tmp.path())
            .with_name("custom.pkg.tar.repo".into())
            .unwrap();
        repo.init().unwrap();
        retire_one(&repo).await;
        let dir = repo.root().join("x86_64");
        let databases = ["db", "files", "db.tar.gz", "files.tar.gz"]
            .map(|suffix| dir.join(format!("{}.{suffix}", repo.name())));
        for database in &databases {
            std::fs::write(with_suffix(database, ".sig"), b"database signature").unwrap();
        }
        let db = aurcache_db::testing::memory_db().await;
        let grace = Duration::from_secs(60);
        let now = Instant::now();
        assert_eq!(repo.sweep_at(&db, now, grace).await.unwrap(), 0);
        assert_eq!(repo.sweep_at(&db, now + grace, grace).await.unwrap(), 2);
        for database in databases {
            assert!(database.is_file(), "{}", database.display());
            assert!(with_suffix(&database, ".sig").is_file());
        }
        assert_eq!(listed(&repo), ["foo-1.1-1"]);
        assert!(platform_file(&repo, "foo", "1.1-1").exists());
        assert!(!platform_file(&repo, "foo", "1.0-1").exists());
    }

    #[tokio::test]
    async fn changing_the_name_does_not_silently_empty_an_existing_repository() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::new(tmp.path());
        repo.init().unwrap();
        retire_one(&repo).await;
        let renamed = Repository::new(tmp.path())
            .with_name("custom_name".into())
            .unwrap();
        assert!(
            renamed
                .init()
                .unwrap_err()
                .to_string()
                .contains("differently named repository")
        );
        assert_eq!(listed(&repo), ["foo-1.1-1"]);
        for platform in Platform::ALL {
            let dir = Repository::platform_dir(tmp.path(), platform);
            assert!(!dir.join("custom_name.db.tar.gz").exists());
            for suffix in ["db", "files"] {
                std::fs::rename(
                    dir.join(format!("repo.{suffix}.tar.gz")),
                    dir.join(format!("custom_name.{suffix}.tar.gz")),
                )
                .unwrap();
                std::fs::remove_file(dir.join(format!("repo.{suffix}"))).unwrap();
            }
        }
        renamed.init().unwrap();
        assert_eq!(listed(&renamed), ["foo-1.1-1"]);
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

        assert_eq!(
            repo.sweep(&aurcache_db::testing::memory_db().await, grace)
                .await
                .unwrap(),
            0
        );
        assert!(old.exists() && with_suffix(&old, ".sig").exists());

        let later = Instant::now() + grace;
        assert_eq!(
            repo.sweep_at(&aurcache_db::testing::memory_db().await, later, grace)
                .await
                .unwrap(),
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
        assert_eq!(
            restarted
                .sweep(&aurcache_db::testing::memory_db().await, grace)
                .await
                .unwrap(),
            0
        );
        assert!(old.exists());

        let later = Instant::now() + grace;
        assert_eq!(
            restarted
                .sweep_at(&aurcache_db::testing::memory_db().await, later, grace)
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
            repo.name(),
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
            repo.name(),
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
        repo.sweep_at(
            &aurcache_db::testing::memory_db().await,
            later,
            Duration::from_secs(3600),
        )
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
            repo.sweep_at(
                &aurcache_db::testing::memory_db().await,
                later,
                Duration::from_secs(3600)
            )
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
        let db = aurcache_db::testing::memory_db().await;
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
        let db = aurcache_db::testing::memory_db().await;
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
        let db = aurcache_db::testing::memory_db().await;
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

    /// Republished at the same name without a signature, a package loses the
    /// one it had: it signs another file now.
    #[tokio::test]
    async fn an_unsigned_replacement_takes_the_old_signature_with_it() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::new(tmp.path());
        let signature = with_suffix(&platform_file(&repo, "foo", "1.0-1"), ".sig");

        let (staged, entry) = stage(&repo, 1, "foo", "1.0-1").await;
        std::fs::write(with_suffix(&staged, ".sig"), b"first").unwrap();
        apply(&repo, vec![(staged, entry)], &[]).await.unwrap();
        assert_eq!(std::fs::read(&signature).unwrap(), b"first");

        let (staged, entry) = stage(&repo, 2, "foo", "1.0-1").await;
        std::fs::write(with_suffix(&staged, ".sig"), b"second").unwrap();
        apply(&repo, vec![(staged, entry)], &[]).await.unwrap();
        assert_eq!(std::fs::read(&signature).unwrap(), b"second");

        let (staged, entry) = stage(&repo, 3, "foo", "1.0-1").await;
        apply(&repo, vec![(staged, entry)], &[]).await.unwrap();
        assert!(!signature.exists());
        assert!(platform_file(&repo, "foo", "1.0-1").is_file());
        assert_eq!(listed(&repo), ["foo-1.0-1"]);
    }

    /// Removing the old signature is part of the recorded plan: one that
    /// could not be removed is removed when the plan is replayed.
    #[tokio::test]
    async fn a_signature_that_would_not_go_is_removed_on_replay() {
        let tmp = tempfile::tempdir().unwrap();
        let db = aurcache_db::testing::memory_db().await;
        let repo = Repository::new(tmp.path());
        let signature = with_suffix(&platform_file(&repo, "foo", "1.0-1"), ".sig");
        let (staged, entry) = stage(&repo, 1, "foo", "1.0-1").await;
        std::fs::write(with_suffix(&staged, ".sig"), b"first").unwrap();
        apply(&repo, vec![(staged, entry)], &[]).await.unwrap();
        // Something that will not be removed as a file, where it is.
        std::fs::remove_file(&signature).unwrap();
        std::fs::create_dir(&signature).unwrap();
        std::fs::write(signature.join("x"), b"").unwrap();

        let (staged, entry) = stage(&repo, 2, "foo", "1.0-1").await;
        let mut update = repo.begin(&db).await.unwrap();
        update.add(Platform::X86_64, staged, entry);
        let error = update
            .commit(&db, |_| Box::pin(async { Ok(()) }))
            .await
            .unwrap_err();
        assert!(error.downcast_ref::<FinalizationPending>().is_some());

        std::fs::remove_dir_all(&signature).unwrap();
        std::fs::write(&signature, b"first").unwrap();
        let restarted = Repository::new(tmp.path());
        restarted.recover(&db).await.unwrap();
        assert!(!signature.exists());
        assert!(plan_row::read(&db).await.unwrap().is_none());
    }

    /// Replayed, a removal already done is no failure: its directory is still
    /// synced, in case the removal before the restart never reached the disk.
    #[test]
    fn a_removal_already_done_replays() {
        let tmp = tempfile::tempdir().unwrap();
        let plan = Finalization {
            moves: Vec::new(),
            retired: Vec::new(),
            removed: vec![tmp.path().join("foo-1.0-1-x86_64.pkg.tar.zst.sig")],
        };
        plan.finish(&ActivityLog::discarding()).unwrap();
    }

    /// A plan without a `removed` list reads as removing nothing.
    #[test]
    fn a_plan_without_removals_reads() {
        let plan: Finalization = serde_json::from_str(r#"{"moves":[],"retired":[]}"#).unwrap();
        assert!(plan.removed.is_empty());
    }

    /// Two updates started together on one platform both land: the second
    /// starts from the databases the first wrote, not from the ones before it.
    #[tokio::test]
    async fn concurrent_updates_do_not_lose_each_other() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Arc::new(Repository::new(tmp.path()));
        let db = aurcache_db::testing::memory_db().await;
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
        let db = aurcache_db::testing::memory_db().await;
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
            "custom_name.db.tar.gz".to_string(),
            "custom_name.files.tar.gz".to_string(),
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let db = aurcache_db::testing::memory_db().await;
            let repo = Repository::new(tmp.path())
                .with_name("custom_name".into())
                .unwrap();
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
            let restarted = Repository::new(tmp.path())
                .with_name("custom_name".into())
                .unwrap();
            assert!(restarted.begin(&db).await.is_err());
            assert!(restarted.sweep(&db, Duration::ZERO).await.is_err());
            std::fs::remove_dir(&blocked).unwrap();
            restarted.recover(&db).await.unwrap();
            restarted.recover(&db).await.unwrap();
            assert_eq!(listed(&restarted), ["foo-1.0-1"]);
            assert!(platform_file(&restarted, "foo", "1.0-1").is_file());
            assert!(
                tmp.path()
                    .join("x86_64")
                    .join("custom_name.files.tar.gz")
                    .is_file()
            );
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
        let db = aurcache_db::testing::memory_db().await;
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

    fn signing_repo(root: &Path) -> Repository {
        Repository::new(root).with_key(InstanceKey::generate(None).unwrap())
    }

    /// Whether the file at `path` carries the repository key's signature
    /// beside it, over what it is now.
    fn signed(repo: &Repository, path: &Path) -> bool {
        let Ok(signature) = std::fs::read(with_suffix(path, ".sig")) else {
            return false;
        };
        repo.key()
            .unwrap()
            .verifies(&signature, &std::fs::read(path).unwrap())
    }

    fn archive(repo: &Repository, name: &str) -> PathBuf {
        repo.root().join("x86_64").join(name)
    }

    /// A repository with a name of its own signs under that name: the links
    /// pacman fetches the signatures through, the signatures an update
    /// writes and the ones signed in place all carry it, and the sweep keeps
    /// them all.
    #[tokio::test]
    async fn a_named_repository_signs_under_its_name() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::new(tmp.path())
            .with_name("custom_name".into())
            .unwrap()
            .with_key(InstanceKey::generate(None).unwrap());
        repo.init().unwrap();
        let db = aurcache_db::testing::memory_db().await;
        let dir = tmp.path().join("x86_64");
        for suffix in ["db", "files"] {
            assert_eq!(
                std::fs::read_link(dir.join(format!("custom_name.{suffix}.sig"))).unwrap(),
                PathBuf::from(format!("custom_name.{suffix}.tar.gz.sig"))
            );
        }
        repo.sign_databases(&db).await.unwrap();
        assert!(signed(&repo, &dir.join("custom_name.db.tar.gz")));

        let (staged, entry) = stage(&repo, 1, "foo", "1.0-1").await;
        apply(&repo, vec![(staged, entry)], &[]).await.unwrap();
        for database in ["custom_name.db.tar.gz", "custom_name.files.tar.gz"] {
            assert!(signed(&repo, &dir.join(database)), "{database}");
        }
        assert!(signed(&repo, &platform_file(&repo, "foo", "1.0-1")));
        assert!(repo.unsigned(Platform::X86_64).await.unwrap().is_empty());
        assert!(
            !std::fs::read_dir(&dir).unwrap().any(|entry| entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("repo.")),
            "something carries the default name"
        );

        let grace = Duration::from_secs(60);
        let later = Instant::now() + grace * 2;
        assert_eq!(repo.sweep_at(&db, later, grace).await.unwrap(), 0);
        for suffix in ["db", "files"] {
            assert!(
                dir.join(format!("custom_name.{suffix}.tar.gz.sig"))
                    .is_file()
            );
            assert!(dir.join(format!("custom_name.{suffix}.sig")).exists());
        }
    }

    /// Renaming a signed repository renames its signatures with its
    /// databases: a platform where only a signature of the old name is left
    /// is refused like one with the old database, and renamed whole, it signs
    /// under the new name with nothing to sign again.
    #[tokio::test]
    async fn a_signed_repository_is_renamed_with_its_signatures() {
        let tmp = tempfile::tempdir().unwrap();
        let key_path = tmp.path().join("key.asc");
        InstanceKey::load_or_create(&key_path, None).unwrap();
        let key = || InstanceKey::load(&key_path, None).unwrap();
        let repo = Repository::new(tmp.path()).with_key(key());
        repo.init().unwrap();
        let db = aurcache_db::testing::memory_db().await;
        let (staged, entry) = stage(&repo, 1, "foo", "1.0-1").await;
        apply(&repo, vec![(staged, entry)], &[]).await.unwrap();
        repo.sign_databases(&db).await.unwrap();

        let dir = tmp.path().join("x86_64");
        let moved = tmp.path().join("elsewhere");
        std::fs::create_dir(&moved).unwrap();
        std::fs::rename(dir.join(DB_ARCHIVE), moved.join(DB_ARCHIVE)).unwrap();
        let renamed = || {
            Repository::new(tmp.path())
                .with_name("custom_name".into())
                .unwrap()
                .with_key(key())
        };
        let error = renamed().init().unwrap_err().to_string();
        assert!(error.contains("their signatures"), "{error}");
        std::fs::rename(moved.join(DB_ARCHIVE), dir.join(DB_ARCHIVE)).unwrap();

        for platform in Platform::ALL {
            let dir = Repository::platform_dir(tmp.path(), platform);
            for suffix in ["db", "files"] {
                for archive in [format!("{suffix}.tar.gz"), format!("{suffix}.tar.gz.sig")] {
                    let old = dir.join(format!("repo.{archive}"));
                    if old.exists() {
                        std::fs::rename(&old, dir.join(format!("custom_name.{archive}"))).unwrap();
                    }
                }
                for link in [format!("repo.{suffix}"), format!("repo.{suffix}.sig")] {
                    let _ = std::fs::remove_file(dir.join(link));
                }
            }
        }
        let repo = renamed();
        repo.init().unwrap();
        assert!(signed(&repo, &dir.join("custom_name.db.tar.gz")));
        assert!(repo.unsigned(Platform::X86_64).await.unwrap().is_empty());
        assert_eq!(listed(&repo), ["foo-1.0-1"]);
    }

    /// A signing repository publishes each package with its detached
    /// signature, the same signature in its entry, and each database with a
    /// signature over what it is now.
    #[tokio::test]
    async fn a_signed_commit_signs_each_package_and_database() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = signing_repo(tmp.path());
        let (staged, entry) = stage(&repo, 1, "foo", "1.0-1").await;
        apply(&repo, vec![(staged, entry)], &[]).await.unwrap();

        let package = platform_file(&repo, "foo", "1.0-1");
        assert!(signed(&repo, &package));
        assert!(signed(&repo, &archive(&repo, DB_ARCHIVE)));
        assert!(signed(&repo, &archive(&repo, FILES_ARCHIVE)));
        let entries = pacman_repo_utils::listed_packages(&archive(&repo, DB_ARCHIVE)).unwrap();
        assert_eq!(
            entries[0].signature,
            Some(std::fs::read(with_suffix(&package, ".sig")).unwrap())
        );
        assert!(next_files_left(&repo).is_empty());

        // A second update signs the databases it rewrites again.
        let (staged, entry) = stage(&repo, 2, "bar", "1.0-1").await;
        apply(&repo, vec![(staged, entry)], &[]).await.unwrap();
        assert!(signed(&repo, &archive(&repo, DB_ARCHIVE)));
        assert!(signed(&repo, &archive(&repo, FILES_ARCHIVE)));
    }

    /// A repository that does not sign writes no signature of its own.
    #[tokio::test]
    async fn an_unsigned_commit_writes_no_signature() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = Repository::new(tmp.path());
        let (staged, entry) = stage(&repo, 1, "foo", "1.0-1").await;
        apply(&repo, vec![(staged, entry)], &[]).await.unwrap();

        let package = platform_file(&repo, "foo", "1.0-1");
        assert!(!with_suffix(&package, ".sig").exists());
        assert!(!with_suffix(&archive(&repo, DB_ARCHIVE), ".sig").exists());
        let entries = pacman_repo_utils::listed_packages(&archive(&repo, DB_ARCHIVE)).unwrap();
        assert_eq!(entries[0].signature, None);
    }

    /// A signed commit that fails for good leaves every signature as it was,
    /// beside the databases it signs.
    #[tokio::test]
    async fn a_failed_signed_commit_leaves_the_signatures_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = signing_repo(tmp.path());
        let db = aurcache_db::testing::memory_db().await;
        let (staged, entry) = stage(&repo, 1, "foo", "1.0-1").await;
        apply(&repo, vec![(staged, entry)], &[]).await.unwrap();
        let before: Vec<Vec<u8>> = [DB_ARCHIVE, FILES_ARCHIVE]
            .into_iter()
            .flat_map(|name| {
                [
                    archive(&repo, name),
                    with_suffix(&archive(&repo, name), ".sig"),
                ]
            })
            .map(|path| std::fs::read(path).unwrap())
            .collect();

        let (staged, entry) = stage(&repo, 2, "bar", "1.0-1").await;
        let mut update = repo.begin(&db).await.unwrap();
        update.add(Platform::X86_64, staged, entry);
        let failed: anyhow::Result<()> = update
            .commit(&db, |_| {
                Box::pin(async { anyhow::bail!("the database said no") })
            })
            .await;

        assert!(failed.is_err());
        let after: Vec<Vec<u8>> = [DB_ARCHIVE, FILES_ARCHIVE]
            .into_iter()
            .flat_map(|name| {
                [
                    archive(&repo, name),
                    with_suffix(&archive(&repo, name), ".sig"),
                ]
            })
            .map(|path| std::fs::read(path).unwrap())
            .collect();
        assert_eq!(after, before);
        assert!(!platform_file(&repo, "bar", "1.0-1").exists());
        assert!(next_files_left(&repo).is_empty());
    }

    /// A database signature that could not be moved in is moved in by the
    /// recovery, beside the database it signs.
    #[tokio::test]
    async fn a_replayed_plan_moves_the_database_signature_in_too() {
        let tmp = tempfile::tempdir().unwrap();
        let db = aurcache_db::testing::memory_db().await;
        let repo = signing_repo(tmp.path());
        let (staged, entry) = stage(&repo, 1, "foo", "1.0-1").await;
        let blocked = with_suffix(&archive(&repo, DB_ARCHIVE), ".sig");
        let mut update = repo.begin(&db).await.unwrap();
        update.add(Platform::X86_64, staged, entry);
        let error = update
            .commit(&db, |_| {
                let blocked = blocked.clone();
                Box::pin(async move {
                    std::fs::create_dir_all(&blocked)?;
                    Ok(())
                })
            })
            .await
            .unwrap_err();
        assert!(error.downcast_ref::<FinalizationPending>().is_some());
        assert!(archive(&repo, DB_ARCHIVE).is_file(), "the database moved");

        std::fs::remove_dir(&blocked).unwrap();
        repo.recover(&db).await.unwrap();
        assert!(signed(&repo, &archive(&repo, DB_ARCHIVE)));
        assert!(signed(&repo, &archive(&repo, FILES_ARCHIVE)));
        assert!(next_files_left(&repo).is_empty());
    }

    /// A signing repository links the signatures where pacman asks for them;
    /// one that has stopped signing removes them, since its next update
    /// leaves them signing databases that are not there any more.
    #[tokio::test]
    async fn init_links_database_signatures_or_removes_them() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = signing_repo(tmp.path());
        repo.init().unwrap();
        let (staged, entry) = stage(&repo, 1, "foo", "1.0-1").await;
        apply(&repo, vec![(staged, entry)], &[]).await.unwrap();
        let link = archive(&repo, "repo.db.sig");
        assert_eq!(
            std::fs::read(&link).unwrap(),
            std::fs::read(with_suffix(&archive(&repo, DB_ARCHIVE), ".sig")).unwrap()
        );

        Repository::new(tmp.path()).init().unwrap();
        assert!(link.symlink_metadata().is_err());
        assert!(!with_suffix(&archive(&repo, DB_ARCHIVE), ".sig").exists());
        assert!(!with_suffix(&archive(&repo, FILES_ARCHIVE), ".sig").exists());
        assert!(
            with_suffix(&platform_file(&repo, "foo", "1.0-1"), ".sig").exists(),
            "a package's signature still signs it"
        );
    }

    /// Databases written before the repository signed are signed in place,
    /// once: a signature that already verifies is left as it is.
    #[tokio::test]
    async fn databases_are_signed_in_place_once() {
        let tmp = tempfile::tempdir().unwrap();
        let db = aurcache_db::testing::memory_db().await;
        let unsigned = Repository::new(tmp.path());
        unsigned.init().unwrap();
        let (staged, entry) = stage(&unsigned, 1, "foo", "1.0-1").await;
        apply(&unsigned, vec![(staged, entry)], &[]).await.unwrap();

        let repo = signing_repo(tmp.path());
        repo.init().unwrap();
        repo.sign_databases(&db).await.unwrap();
        let signature = with_suffix(&archive(&repo, DB_ARCHIVE), ".sig");
        assert!(signed(&repo, &archive(&repo, DB_ARCHIVE)));
        assert!(signed(&repo, &archive(&repo, FILES_ARCHIVE)));
        let first = std::fs::read(&signature).unwrap();

        repo.sign_databases(&db).await.unwrap();
        assert_eq!(std::fs::read(&signature).unwrap(), first);
        assert!(
            !std::fs::read_dir(repo.root().join("x86_64"))
                .unwrap()
                .flatten()
                .any(|e| e.file_name().to_string_lossy().ends_with(".partial"))
        );
    }

    /// A worker's own `.sig` for a package never reaches the repository of a
    /// server that signs: describing the package replaces it.
    #[tokio::test]
    async fn the_servers_signature_replaces_an_uploaded_one() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = signing_repo(tmp.path());
        let dir = repo.staging_dir(1);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(filename("foo", "1.0-1"));
        std::fs::write(&path, fake_pkg_zst("foo", "1.0-1")).unwrap();
        std::fs::write(with_suffix(&path, ".sig"), b"uploaded").unwrap();

        let entry = repo.describe(path.clone()).await.unwrap();
        apply(&repo, vec![(path, entry)], &[]).await.unwrap();
        assert!(signed(&repo, &platform_file(&repo, "foo", "1.0-1")));
    }

    #[test]
    fn a_database_signature_is_written_beside_the_next_database() {
        assert_eq!(
            signature_next(Path::new("/r/x86_64/.repo.db.tar.gz.next")),
            Path::new("/r/x86_64/.repo.db.tar.gz.sig.next")
        );
    }
}
