//! Publishing packages built elsewhere.
//!
//! An upload is a build that no worker ran: its files arrive with the request,
//! and from there it is published exactly as a worker's build is
//! ([`crate::publish`]). What is particular to an upload is all before that --
//! holding the files until they are known to be complete, and deciding whether
//! they may be published under the package at all.
//!
//! The files are held under the staging root, in a directory of their own,
//! until every one has arrived and passed. Only then is the build row written
//! (straight into `PUBLISHING`) and the directory renamed into that build's
//! staging directory. A build is therefore never `PUBLISHING` with only some
//! of its files staged: a restart either finds the whole upload or nothing,
//! and nothing fails the build rather than publishing part of it.

use crate::publish::{
    Artifact, PublicationOutcome, artifact, expected_pkgnames, log, parse_arch_pkg, publish_build,
    publish_with_recovery,
};
use crate::repository::Repository;
use aurcache_activitylog::activity_utils::ActivityLog;
use aurcache_activitylog::events::Event;
use aurcache_common::api::log::BuildRef;
use aurcache_common::api::package::UploadedBuild;
use aurcache_common::build_state::{BuildState, BuildTrigger};
use aurcache_common::upload::FileKind;
use aurcache_db::helpers::build_enqueue::{Pending, Queued, enqueue_build_if_missing};
use aurcache_db::helpers::time::now_secs;
use aurcache_db::packages;
use aurcache_db::prelude::Builds;
use pacman_mirrors::platforms::Platform;
use sea_orm::{DatabaseConnection, EntityTrait};
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use tracing::warn;

/// The largest detached signature accepted, as `repo-add` has it.
pub const MAX_SIGNATURE_BYTES: u64 = pacman_repo_utils::MAX_SIGNATURE_BYTES as u64;

/// The most files one upload may carry, signatures included.
const MAX_FILES: usize = 64;

/// The prefix of a holding directory's name, under the staging root.
///
/// Build staging directories are named by build id, so a name that is not a
/// number is never taken for one.
const HOLDING_PREFIX: &str = "upload-";

/// Why an upload was not accepted.
#[derive(Debug)]
pub enum UploadError {
    /// The request or its files are not something that can be published
    /// under this package; the message says which file and why.
    Refused(String),
    /// A build of the package on that platform has not settled yet, and an
    /// upload does not overtake it.
    Busy(BuildRef),
    /// The server's own failure.
    Internal(anyhow::Error),
}

impl fmt::Display for UploadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refused(reason) => f.write_str(reason),
            Self::Busy(build) => write!(
                f,
                "{}/{} has not finished yet; cancel it or wait for it before uploading",
                build.pkgbase, build.number
            ),
            Self::Internal(e) => write!(f, "{e:#}"),
        }
    }
}

impl std::error::Error for UploadError {}

impl From<sea_orm::DbErr> for UploadError {
    fn from(e: sea_orm::DbErr) -> Self {
        Self::Internal(e.into())
    }
}

impl From<std::io::Error> for UploadError {
    fn from(e: std::io::Error) -> Self {
        Self::Internal(e.into())
    }
}

fn refused(reason: impl Into<String>) -> UploadError {
    UploadError::Refused(reason.into())
}

/// The package file `filename` is or signs.
fn package_of(filename: &str) -> &str {
    filename.strip_suffix(".sig").unwrap_or(filename)
}

/// The pkgname of the package file `filename` is or signs.
fn pkgname(filename: &str) -> Option<String> {
    parse_arch_pkg(package_of(filename))
        .ok()
        .map(|named| named.name)
}

fn not_configured(pkg: &packages::Model, platform: Platform) -> UploadError {
    refused(format!(
        "{} is not configured for {platform}; add the platform to it first",
        pkg.name
    ))
}

/// The files of one upload as they arrive, held apart until they are all in.
///
/// Removed when dropped, so a request that fails part-way leaves nothing
/// behind; one the process did not survive is removed by
/// [`discard_abandoned`].
pub struct Received {
    dir: tempfile::TempDir,
    /// The files accepted so far, in the order they arrived.
    files: Vec<String>,
    artifact_limit: u64,
    /// The package the upload is for, and the names its source builds.
    pkgbase: String,
    expected: Vec<String>,
}

/// Where one arriving file is to be written, and how large it may be.
pub struct Incoming {
    pub path: PathBuf,
    pub limit: u64,
}

impl Received {
    /// Start holding the files of an upload to `pkg`, each package at most
    /// `artifact_limit` bytes.
    ///
    /// # Errors
    /// The staging root cannot be written.
    pub async fn new(
        repo: &Repository,
        pkg: &packages::Model,
        artifact_limit: u64,
    ) -> std::io::Result<Self> {
        let root = repo.staging_root();
        let dir = tokio::task::spawn_blocking(move || {
            std::fs::create_dir_all(&root)?;
            tempfile::Builder::new()
                .prefix(HOLDING_PREFIX)
                .tempdir_in(root)
        })
        .await
        .map_err(std::io::Error::other)??;
        Ok(Self {
            dir,
            files: Vec::new(),
            artifact_limit,
            pkgbase: pkg.name.clone(),
            expected: expected_pkgnames(pkg),
        })
    }

    /// Accept a file named `filename` into the upload, and say where to write
    /// it.
    ///
    /// Refusing by name before the bytes are written bounds what an upload
    /// holds on disk to one build's files.
    ///
    /// # Errors
    /// [`UploadError::Refused`] for a name an upload cannot carry.
    pub fn accept(&mut self, filename: &str) -> Result<Incoming, UploadError> {
        let kind = FileKind::of(filename).map_err(refused)?;
        if artifact(&self.expected, package_of(filename)).is_err() {
            return Err(refused(format!(
                "{filename} is not among the packages {}'s source builds: {}",
                self.pkgbase,
                self.expected.join(", ")
            )));
        }
        if self.files.len() >= MAX_FILES {
            return Err(refused(format!(
                "an upload carries at most {MAX_FILES} files"
            )));
        }
        let name = pkgname(filename);
        if self
            .files
            .iter()
            .any(|file| FileKind::of(file) == Ok(kind) && pkgname(file) == name)
        {
            let what = match kind {
                FileKind::Package => "version",
                FileKind::Signature => "signature",
            };
            return Err(refused(format!(
                "{filename}: the upload already carries a {what} of that package"
            )));
        }
        self.files.push(filename.to_string());
        Ok(Incoming {
            path: self.dir.path().join(filename),
            limit: match kind {
                FileKind::Package => self.artifact_limit,
                FileKind::Signature => MAX_SIGNATURE_BYTES,
            },
        })
    }

    /// The files accepted so far, in the order they arrived.
    #[must_use]
    pub fn files(&self) -> &[String] {
        &self.files
    }

    /// Where the files are held.
    #[must_use]
    pub fn path(&self) -> &Path {
        self.dir.path()
    }
}

/// Remove the holding directories of uploads a restart interrupted.
///
/// Run at startup, before the API takes requests: nothing can be writing to
/// one then, and nothing ever reads one again.
pub async fn discard_abandoned(repo: &Repository) -> usize {
    let Ok(mut entries) = tokio::fs::read_dir(repo.staging_root()).await else {
        return 0;
    };
    let mut removed = 0;
    while let Ok(Some(entry)) = entries.next_entry().await {
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with(HOLDING_PREFIX)
            && tokio::fs::remove_dir_all(entry.path()).await.is_ok()
        {
            removed += 1;
        }
    }
    removed
}

/// What an upload is for, besides its files.
#[derive(Clone, Debug)]
pub struct UploadRequest {
    /// The package it is a build of.
    pub pkg: packages::Model,
    /// The platform it is for, when its files do not say: an upload of only
    /// `arch = any` packages.
    pub platform: Option<Platform>,
    /// Who uploaded it, when the instance knows.
    pub uploader: Option<String>,
}

/// An upload's files, checked against each other by their names: one
/// build's worth.
struct Contents {
    /// The package files publishing reads, makepkg's `-debug` ones left out.
    packages: Vec<String>,
    version: String,
    /// The one platform its files name, if any names one rather than `any`.
    arch: Option<Platform>,
}

/// Check an upload's files against each other, before anything is recorded.
///
/// Only names are read: what a file holds is publishing's to check, as for a
/// worker's build.
fn inspect(received: &Received) -> Result<Contents, UploadError> {
    let mut packages = Vec::new();
    for filename in received.files() {
        match FileKind::of(filename) {
            Ok(FileKind::Package) => {
                if let Ok(Artifact::Package(_)) = artifact(&received.expected, filename) {
                    packages.push(filename.clone());
                }
            }
            _ => {
                let signed = package_of(filename);
                if !received.files().iter().any(|file| file == signed) {
                    return Err(refused(format!(
                        "'{filename}' signs '{signed}', which is not in the upload"
                    )));
                }
            }
        }
    }
    if packages.is_empty() {
        return Err(refused("the upload carries no package to publish"));
    }
    check(packages)
}

/// Check that `packages` are one build by their names: one version and one
/// platform.
fn check(packages: Vec<String>) -> Result<Contents, UploadError> {
    let mut version: Option<String> = None;
    let mut arch: Option<Platform> = None;
    for filename in &packages {
        let named = parse_arch_pkg(filename).map_err(|e| refused(e.to_string()))?;
        match &version {
            None => version = Some(named.version.clone()),
            Some(v) if *v == named.version => {}
            Some(v) => {
                return Err(refused(format!(
                    "{filename} is version {}, the rest of the upload {v}",
                    named.version
                )));
            }
        }
        if named.arch != "any" {
            let platform = Platform::from_str(&named.arch).map_err(|_| {
                refused(format!(
                    "{filename} is for {}, which is not served here",
                    named.arch
                ))
            })?;
            match arch {
                None => arch = Some(platform),
                Some(a) if a == platform => {}
                Some(a) => {
                    return Err(refused(format!(
                        "{filename} is for {platform}, the rest of the upload for {a}"
                    )));
                }
            }
        }
    }
    Ok(Contents {
        packages,
        version: version.unwrap_or_default(),
        arch,
    })
}

/// The platform an upload is for: the one its files name, else the one the
/// request names, else the only one the package is `configured` for.
fn platform(
    contents: &Contents,
    requested: Option<Platform>,
    configured: &[Platform],
) -> Result<Platform, UploadError> {
    match (contents.arch, requested) {
        (Some(arch), Some(requested)) if arch != requested => Err(refused(format!(
            "the files are for {arch}, not {requested}"
        ))),
        (Some(arch), _) => Ok(arch),
        (None, Some(requested)) => Ok(requested),
        (None, None) => match configured {
            [only] => Ok(*only),
            _ => Err(refused(
                "every file is 'arch = any', and the package is built for several platforms: \
                 name the one the upload is for (aurcli pkg upload --platform)",
            )),
        },
    }
}

/// Publish an upload as a build of `request.pkg`.
///
/// Waits for the first attempt to publish it, and answers with where that
/// left it; a recoverable failure carries on in the background, as for a
/// worker's build.
///
/// # Errors
/// [`UploadError::Refused`] when the files cannot be published under the
/// package, [`UploadError::Busy`] when a build of it has not settled on that
/// platform; in each case nothing is recorded.
pub async fn publish_upload(
    db: &DatabaseConnection,
    repo: &Arc<Repository>,
    activity: &ActivityLog,
    request: UploadRequest,
    received: Received,
) -> Result<UploadedBuild, UploadError> {
    let pkg = &request.pkg;
    let contents = inspect(&received)?;
    let platform = platform(&contents, request.platform, pkg.platforms.as_slice())?;
    if !pkg.platforms.contains(platform) {
        return Err(not_configured(pkg, platform));
    }

    // Run in its own task: if the request is dropped, this future could stop
    // between writing the row and moving the files.
    let (db, repo, activity) = (db.clone(), Arc::clone(repo), activity.clone());
    tokio::spawn(async move {
        record_and_publish(&db, &repo, &activity, request, received, contents, platform).await
    })
    .await
    .map_err(|e| UploadError::Internal(e.into()))?
}

/// Record the build of an upload that passed every check, stage its files
/// and publish it.
async fn record_and_publish(
    db: &DatabaseConnection,
    repo: &Arc<Repository>,
    activity: &ActivityLog,
    request: UploadRequest,
    received: Received,
    contents: Contents,
    platform: Platform,
) -> Result<UploadedBuild, UploadError> {
    let pkgbase = request.pkg.name.as_str();
    let recorded = enqueue_build_if_missing(
        db,
        request.pkg.id,
        platform,
        &contents.version,
        now_secs(),
        Pending::Publishing,
        BuildTrigger::Upload,
    )
    .await?;
    let build = recorded.build;
    let named = BuildRef {
        pkgbase: pkgbase.to_string(),
        number: build.number,
    };
    if recorded.queued == Queued::Existing {
        return Err(UploadError::Busy(named));
    }
    log_upload(
        activity,
        &named,
        request.uploader.as_deref(),
        received.files(),
    )
    .await;

    // Until this rename the build has no staging directory, so a failed
    // rename makes publishing fail the build. Dropping `received` then removes
    // the files only if they were not moved.
    match tokio::fs::rename(received.path(), repo.staging_dir(build.id)).await {
        Ok(()) => activity.emit_by(
            Event::BuildUploaded {
                build: named,
                version: contents.version.clone(),
                files: contents.packages,
            },
            request.uploader,
        ),
        Err(e) => {
            warn!("could not stage the upload of build #{}: {e}", build.id);
            log(
                Some(activity),
                Some(pkgbase),
                build.number,
                &format!("Could not stage the uploaded files: {e}\n"),
            )
            .await;
        }
    }
    drop(received);

    if publish_build(db, repo, activity, build.id).await == PublicationOutcome::Retry {
        let (db, repo, activity) = (db.clone(), Arc::clone(repo), activity.clone());
        let build_id = build.id;
        tokio::spawn(async move {
            publish_with_recovery(&db, &repo, &activity, build_id).await;
        });
    }
    let status = Builds::find_by_id(build.id)
        .one(db)
        .await?
        .map_or(BuildState::Failed, |build| build.status);
    Ok(UploadedBuild {
        pkgbase: pkgbase.to_string(),
        number: build.number,
        platform: platform.to_string(),
        version: contents.version,
        status,
    })
}

/// Open the build's log with who uploaded what.
async fn log_upload(
    activity: &ActivityLog,
    build: &BuildRef,
    uploader: Option<&str>,
    files: &[String],
) {
    let mut text = match uploader {
        Some(user) => format!("Uploaded by {user}:\n"),
        None => "Uploaded:\n".to_string(),
    };
    for file in files {
        text.push_str(&format!("  {file}\n"));
    }
    log(Some(activity), Some(&build.pkgbase), build.number, &text).await;
}
