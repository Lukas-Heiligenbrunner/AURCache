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
use anyhow::anyhow;
use aurcache_activitylog::activity_utils::ActivityLog;
use aurcache_activitylog::events::Event;
use aurcache_common::api::log::BuildRef;
use aurcache_common::api::package::UploadedBuild;
use aurcache_common::build_state::BuildState;
use aurcache_common::upload::{File, FileKind, Names};
use aurcache_db::helpers::build_enqueue::{UploadSlot, record_upload};
use aurcache_db::helpers::time::now_secs;
use aurcache_db::packages;
use aurcache_db::prelude::{Builds, Packages};
use pacman_mirrors::platforms::Platform;
use sea_orm::{DatabaseConnection, EntityTrait, QuerySelect};
use std::collections::HashSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use tracing::warn;

/// The largest detached signature accepted, as `repo-add` has it.
pub const MAX_SIGNATURE_BYTES: u64 = pacman_repo_utils::MAX_SIGNATURE_BYTES as u64;

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
    /// Another request removed the package while this one was being
    /// checked.
    Contended(String),
    /// The server's own failure.
    Internal(anyhow::Error),
}

impl fmt::Display for UploadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refused(reason) | Self::Contended(reason) => f.write_str(reason),
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

/// The file `filename` names, when it is one an upload may carry
/// ([`File::new`]), for a version as makepkg writes one ([`valid_version`]).
fn upload_file(filename: &str) -> Result<File, UploadError> {
    let file = File::new(filename).map_err(refused)?;
    let named = parse_arch_pkg(package_of(filename)).map_err(|e| refused(e.to_string()))?;
    if !valid_version(&named.version) {
        return Err(refused(format!(
            "'{filename}' is for version '{}', which is not [epoch:]pkgver-pkgrel \
             as makepkg writes one",
            named.version
        )));
    }
    Ok(file)
}

/// The package file `filename` is or signs.
fn package_of(filename: &str) -> &str {
    filename.strip_suffix(".sig").unwrap_or(filename)
}

/// Whether `version` is `[epoch:]pkgver-pkgrel` as makepkg writes one: the
/// epoch digits, the pkgver printable ASCII without whitespace, `/`, `:` or
/// `-`, the pkgrel digits with at most one `.` among them.
#[must_use]
pub fn valid_version(version: &str) -> bool {
    let digits = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
    let Some((upstream, pkgrel)) = version.rsplit_once('-') else {
        return false;
    };
    let pkgver = match upstream.split_once(':') {
        Some((epoch, pkgver)) if digits(epoch) => pkgver,
        Some(_) => return false,
        None => upstream,
    };
    let pkgrel = match pkgrel.split_once('.') {
        Some((major, minor)) => digits(major) && digits(minor),
        None => digits(pkgrel),
    };
    pkgrel
        && !pkgver.is_empty()
        && pkgver
            .bytes()
            .all(|b| b.is_ascii_graphic() && !b"/:-".contains(&b))
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
    /// `None` once kept, or while being dropped.
    dir: Option<tempfile::TempDir>,
    names: Names,
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
            dir: Some(dir),
            names: Names::default(),
            artifact_limit,
            pkgbase: pkg.name.clone(),
            expected: expected_pkgnames(pkg),
        })
    }

    /// Accept a file named `filename` into the upload, and say where to write
    /// it.
    ///
    /// Each package must be one the package's source builds, as a worker's
    /// artifact must (makepkg's `-debug` packages of them included, which
    /// publishing leaves out), and each signature one of such a package. The
    /// names are held to what [`Names`] holds them to, as a client checks
    /// them before sending.
    ///
    /// # Errors
    /// [`UploadError::Refused`] for a name [`upload_file`] refuses, a package
    /// the source does not build, or one [`Names`] refuses: a name already
    /// received, or one file more than
    /// [`MAX_FILES`](aurcache_common::upload::MAX_FILES).
    pub fn accept(&mut self, filename: &str) -> Result<Incoming, UploadError> {
        let file = upload_file(filename)?;
        if artifact(&self.expected, package_of(filename)).is_err() {
            return Err(refused(format!(
                "{filename} is not among the packages {}'s source builds: {}",
                self.pkgbase,
                self.expected.join(", ")
            )));
        }
        let kind = file.kind();
        self.names.take(file).map_err(refused)?;
        Ok(Incoming {
            path: self.path().join(filename),
            limit: match kind {
                FileKind::Package => self.artifact_limit,
                FileKind::Signature => MAX_SIGNATURE_BYTES,
            },
        })
    }

    /// The files accepted so far, in the order they arrived.
    #[must_use]
    pub fn files(&self) -> &[File] {
        self.names.taken()
    }

    /// Where the files are held.
    #[must_use]
    pub fn path(&self) -> &Path {
        self.dir
            .as_ref()
            .map_or_else(|| Path::new(""), tempfile::TempDir::path)
    }

    /// Stop removing the files on drop, and say where they are.
    fn keep(mut self) -> PathBuf {
        self.dir
            .take()
            .map_or_else(PathBuf::new, tempfile::TempDir::keep)
    }
}

impl Drop for Received {
    /// Removing the files reads and deletes whatever was received, possibly
    /// gigabytes: off the async runtime when there is one.
    fn drop(&mut self) {
        if let Some(dir) = self.dir.take() {
            match tokio::runtime::Handle::try_current() {
                Ok(runtime) => drop(runtime.spawn_blocking(move || drop(dir))),
                Err(_) => drop(dir),
            }
        }
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
/// Only their names are read here. What each file holds is publishing's to
/// check, as for a worker's build: a `.PKGINFO` that is not what its
/// filename says, or a signature publishing cannot read, fails the build.
/// makepkg's `-debug` packages are left out, as publishing leaves them out
/// of a worker's build, since the repository does not carry them and
/// `makechrootpkg` writes them by default.
fn inspect(received: &Received) -> Result<Contents, UploadError> {
    let mut packages = Vec::new();
    let mut signatures = Vec::new();
    for file in received.files() {
        let filename = file.name();
        match file.kind() {
            FileKind::Package => {
                if let Ok(Artifact::Package(_)) = artifact(&received.expected, filename) {
                    packages.push(filename.to_string());
                }
            }
            FileKind::Signature => signatures.push(filename),
        }
    }
    if packages.is_empty() {
        return Err(refused("the upload carries no package to publish"));
    }
    for signature in signatures {
        let signed = package_of(signature);
        if !received.files().iter().any(|file| file.name() == signed) {
            return Err(refused(format!(
                "'{signature}' signs '{signed}', which is not in the upload"
            )));
        }
    }
    check(packages)
}

/// Check that `packages` are one build by their names: each package once,
/// one version and one platform.
fn check(packages: Vec<String>) -> Result<Contents, UploadError> {
    let mut version: Option<String> = None;
    let mut arch: Option<Platform> = None;
    let mut pkgnames = HashSet::new();
    for filename in &packages {
        let named = parse_arch_pkg(filename).map_err(|e| refused(e.to_string()))?;
        if !pkgnames.insert(named.name.clone()) {
            return Err(refused(format!(
                "the upload carries '{}' more than once",
                named.name
            )));
        }
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
/// platform, [`UploadError::Contended`] when another request removed the
/// package meanwhile; in each case nothing is recorded.
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

    // From the build row on, the upload is seen through even when whoever
    // sent it stops waiting: in a task of its own rather than in this future,
    // which a dropped request would drop between the row and its files.
    let accepted = Accepted {
        request,
        received,
        contents,
        platform,
    };
    let (db, repo, activity) = (db.clone(), Arc::clone(repo), activity.clone());
    tokio::spawn(async move { accepted.publish(&db, &repo, &activity).await })
        .await
        .map_err(|e| UploadError::Internal(e.into()))?
}

/// An upload that passed every check of its files, about to be recorded.
struct Accepted {
    request: UploadRequest,
    received: Received,
    contents: Contents,
    platform: Platform,
}

impl Accepted {
    /// Record the build, stage its files and publish it.
    async fn publish(
        self,
        db: &DatabaseConnection,
        repo: &Arc<Repository>,
        activity: &ActivityLog,
    ) -> Result<UploadedBuild, UploadError> {
        let Self {
            request,
            received,
            contents,
            platform,
        } = self;
        let pkgbase = request.pkg.name.as_str();
        let build = record(db, &request.pkg, platform, &contents).await?;
        let named = BuildRef {
            pkgbase: pkgbase.to_string(),
            number: build.number,
        };
        let files = contents.packages.clone();
        log_upload(
            activity,
            &named,
            request.uploader.as_deref(),
            received.files(),
        )
        .await;

        // Only now does the build have its files: until the rename, its
        // staging directory does not exist, and publishing it fails it, which
        // is where a failed rename leaves it.
        let held = received.keep();
        let staged = tokio::fs::rename(&held, repo.staging_dir(build.id)).await;
        match &staged {
            Ok(()) => activity.emit_by(
                Event::BuildUploaded {
                    build: named.clone(),
                    version: contents.version.clone(),
                    files,
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
                let _ = tokio::fs::remove_dir_all(&held).await;
            }
        }

        if publish_build(db, repo, activity, build.id).await == PublicationOutcome::Retry {
            let (db, repo, activity) = (db.clone(), Arc::clone(repo), activity.clone());
            let build_id = build.id;
            tokio::spawn(async move {
                publish_with_recovery(&db, &repo, &activity, build_id).await;
            });
        }
        if let Err(e) = staged {
            return Err(UploadError::Internal(
                anyhow!(e).context("could not stage the upload"),
            ));
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
}

/// Record the upload's build, in one transaction with the package's status.
///
/// The package row is read again inside it, locked, so one removed since the
/// upload was checked is told apart from a failure of the server's own.
async fn record(
    db: &DatabaseConnection,
    pkg: &packages::Model,
    platform: Platform,
    contents: &Contents,
) -> Result<aurcache_db::builds::Model, UploadError> {
    let txn = aurcache_db::helpers::txn::begin_write(db).await?;
    if Packages::find_by_id(pkg.id)
        .lock_exclusive()
        .one(&txn)
        .await?
        .is_none()
    {
        return Err(UploadError::Contended(format!(
            "{} was removed by another request meanwhile",
            pkg.name
        )));
    }
    match record_upload(&txn, pkg.id, platform, &contents.version, now_secs()).await? {
        UploadSlot::Recorded(build) => {
            txn.commit().await?;
            Ok(build)
        }
        UploadSlot::Busy(build) => Err(UploadError::Busy(BuildRef {
            pkgbase: pkg.name.clone(),
            number: build.number,
        })),
    }
}

/// Open the build's log with who uploaded what.
async fn log_upload(
    activity: &ActivityLog,
    build: &BuildRef,
    uploader: Option<&str>,
    files: &[File],
) {
    let mut text = match uploader {
        Some(user) => format!("Uploaded by {user}:\n"),
        None => "Uploaded:\n".to_string(),
    };
    for file in files {
        text.push_str(&format!("  {}\n", file.name()));
    }
    log(Some(activity), Some(&build.pkgbase), build.number, &text).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_are_as_makepkg_writes_them() {
        for good in ["1.0-1", "1:2.0-1", "2.0_rc1+r3.gabc-1.1", "20260101-12"] {
            assert!(valid_version(good), "{good}");
        }
        for bad in [
            "1.0",
            "1.0-",
            "-1",
            "a:1.0-1",
            ":1.0-1",
            "1:2:3-1",
            "1.0-a",
            "1.0-1.",
            "1.0-1.2.3",
            "1 0-1",
            "1/0-1",
            "1.0\t-1",
            "1.\u{e9}-1",
        ] {
            assert!(!valid_version(bad), "{bad:?}");
        }
        let reason = match upload_file("hello-1.0-x-x86_64.pkg.tar.zst") {
            Err(UploadError::Refused(reason)) => reason,
            other => panic!("expected a refusal, got {other:?}"),
        };
        assert!(reason.contains("[epoch:]pkgver-pkgrel"), "{reason}");
    }

    fn names(filenames: &[&str]) -> Vec<String> {
        filenames.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn split_packages_are_one_build() {
        let contents = check(names(&[
            "demo-1.0-1-x86_64.pkg.tar.zst",
            "demo-docs-1.0-1-any.pkg.tar.zst",
        ]))
        .unwrap();
        assert_eq!(contents.arch, Some(Platform::X86_64));
        assert_eq!(contents.version, "1.0-1");
    }

    #[test]
    fn one_upload_is_one_build() {
        let two_arches = check(names(&[
            "demo-1.0-1-x86_64.pkg.tar.zst",
            "demo-x-1.0-1-aarch64.pkg.tar.zst",
        ]));
        assert!(matches!(two_arches, Err(UploadError::Refused(_))));

        let two_versions = check(names(&[
            "demo-1.0-1-x86_64.pkg.tar.zst",
            "demo-x-1.0-2-x86_64.pkg.tar.zst",
        ]));
        assert!(matches!(two_versions, Err(UploadError::Refused(_))));

        let twice = check(names(&[
            "demo-1.0-1-x86_64.pkg.tar.zst",
            "demo-1.0-1-any.pkg.tar.zst",
        ]));
        assert!(matches!(twice, Err(UploadError::Refused(_))));

        let unserved = check(names(&["demo-1.0-1-riscv64.pkg.tar.zst"]));
        assert!(matches!(unserved, Err(UploadError::Refused(_))));
    }

    #[test]
    fn an_all_any_upload_needs_a_platform() {
        let contents = check(names(&["demo-1.0-1-any.pkg.tar.zst"])).unwrap();
        let both = [Platform::X86_64, Platform::Aarch64];
        let reason = match platform(&contents, None, &both) {
            Err(UploadError::Refused(reason)) => reason,
            other => panic!("expected a refusal, got {other:?}"),
        };
        assert!(reason.contains("--platform"), "{reason}");
        assert_eq!(
            platform(&contents, Some(Platform::Aarch64), &both).unwrap(),
            Platform::Aarch64
        );
        assert_eq!(
            platform(&contents, None, &[Platform::Aarch64]).unwrap(),
            Platform::Aarch64
        );
    }
}
