//! Uploading packages built elsewhere: what is accepted, what it becomes, and
//! that nothing about an uploaded package ever reaches a worker.

use aurcache_activitylog::activity_utils::ActivityLog;
use aurcache_common::api::worker::ApprovalStatus;
use aurcache_common::build_state::{BuildState, BuildTrigger};
use aurcache_common::source::SourceData;
use aurcache_db::lists::JsonList;
use aurcache_db::prelude::{Builds, Files, Packages};
use aurcache_db::testing::{build, depend, memory_db, package};
use aurcache_db::{builds, packages};
use aurcache_deps::AurClient;
use aurcache_utils::package::enqueue::enqueue_missing_buildable_packages;
use aurcache_utils::package::update::{Rebuild, package_update};
use aurcache_utils::repository::Repository;
use aurcache_utils::services::Services;
use aurcache_utils::snapshot::SnapshotStore;
use aurcache_utils::upload::{Received, UploadError, UploadRequest, publish_upload};
use pacman_mirrors::platforms::Platform;
use sea_orm::{ColumnTrait, ConnectionTrait, DatabaseConnection, EntityTrait, QueryFilter, Set};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Build logs go somewhere disposable, the same place for every test in this
/// process.
///
/// One place for all of them, because the server reads it from the process's
/// environment: a log is keyed by pkgbase and build number alone, so tests
/// running side by side keep their logs apart by each uploading to a package
/// of its own.
fn quiet_logs() -> &'static Path {
    static LOGS: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    LOGS.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap().keep();
        // SAFETY: set once, before any test reads it, and never changed.
        unsafe { std::env::set_var("AURCACHE_BUILD_LOG_PATH", &dir) };
        dir
    })
}

/// One package file of an upload, as makepkg would write its `.PKGINFO`.
struct Pkg<'a> {
    pkgname: &'a str,
    pkgbase: &'a str,
    version: &'a str,
    arch: &'a str,
}

impl Pkg<'_> {
    fn filename(&self) -> String {
        format!(
            "{}-{}-{}.pkg.tar.zst",
            self.pkgname, self.version, self.arch
        )
    }

    fn bytes(&self) -> Vec<u8> {
        zstd::stream::encode_all(self.tar().as_slice(), 0).unwrap()
    }

    /// The package with its archive cut short after the `.PKGINFO`: what it
    /// says it is reads fine, and the archive as a whole does not.
    fn truncated_bytes(&self) -> Vec<u8> {
        let mut tar = self.tar();
        tar.truncate(tar.len() - 1024);
        tar.extend_from_slice(&[b'x'; 512]);
        zstd::stream::encode_all(tar.as_slice(), 0).unwrap()
    }

    fn tar(&self) -> Vec<u8> {
        let pkginfo = format!(
            "pkgname = {}\npkgbase = {}\npkgver = {}\narch = {}\npkgdesc = {} for tests\n\
             url = https://example.org/{}\nlicense = MIT\nprovides = {}-virtual\n",
            self.pkgname,
            self.pkgbase,
            self.version,
            self.arch,
            self.pkgname,
            self.pkgbase,
            self.pkgname
        );
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
        tar_bytes
    }

    /// The package followed by a member said to be a terabyte, of which the
    /// archive holds `zeros` zero bytes: it reads as a package up to its
    /// files, and expands far past what its size allows after that.
    fn expanding_bytes(&self, zeros: u64) -> Vec<u8> {
        let mut tar = self.tar();
        tar.truncate(tar.len() - 1024);
        let mut header = tar::Header::new_gnu();
        header.set_path("usr/share/huge").unwrap();
        header.set_size(1 << 40);
        header.set_mode(0o644);
        header.set_cksum();
        tar.extend_from_slice(header.as_bytes());
        zstd_with_zeros(&tar, zeros)
    }
}

/// A zstd frame of `prefix`, then `zeros` zero bytes as RLE blocks: four
/// bytes of the frame for every 128 KiB of them.
fn zstd_with_zeros(prefix: &[u8], zeros: u64) -> Vec<u8> {
    const BLOCK: usize = 128 << 10;
    // The magic number, no flags, a 128 KiB window.
    let mut frame = vec![0x28, 0xb5, 0x2f, 0xfd, 0x00, 0x38];
    let block_header = |frame: &mut Vec<u8>, rle: bool, size: usize, last: bool| {
        let header = (u32::try_from(size).unwrap() << 3) | (u32::from(rle) << 1) | u32::from(last);
        frame.extend_from_slice(&header.to_le_bytes()[..3]);
    };
    for chunk in prefix.chunks(BLOCK) {
        block_header(&mut frame, false, chunk.len(), false);
        frame.extend_from_slice(chunk);
    }
    let mut left = zeros;
    while left > 0 {
        let size = left.min(BLOCK as u64);
        left -= size;
        block_header(&mut frame, true, usize::try_from(size).unwrap(), left == 0);
        frame.push(0);
    }
    frame
}

/// A detached signature gpg wrote, an Ed25519 key's.
fn signature() -> Vec<u8> {
    const HEX: &str = "88750400160a001d16210420d4c8745c5e82cd7bca0c9cab9df7448d8ab3e5\
                       05026aff3999000a0910ab9df7448d8ab3e519b90100f42ed9977bab07db72\
                       db61998a196a637344fe286e74162f597ce25b7594ec7600fd1f1f03e0e21e\
                       8cc4e7e492834f74b1a31b5bbea1be2c2b09bfb1a84cbe417b0b";
    (0..HEX.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&HEX[at..at + 2], 16).unwrap())
        .collect()
}

/// One file of an upload, as it arrives.
struct File {
    name: String,
    bytes: Vec<u8>,
}

impl From<&Pkg<'_>> for File {
    fn from(pkg: &Pkg<'_>) -> Self {
        Self {
            name: pkg.filename(),
            bytes: pkg.bytes(),
        }
    }
}

fn pkg<'a>(pkgname: &'a str, pkgbase: &'a str, version: &'a str, arch: &'a str) -> Pkg<'a> {
    Pkg {
        pkgname,
        pkgbase,
        version,
        arch,
    }
}

struct Env {
    db: DatabaseConnection,
    repo: Arc<Repository>,
    _root: tempfile::TempDir,
}

async fn env() -> Env {
    quiet_logs();
    let root = tempfile::tempdir().unwrap();
    Env {
        db: memory_db().await,
        repo: Arc::new(Repository::new(root.path())),
        _root: root,
    }
}

impl Env {
    /// Receive `files` the way the API does, then publish them as a build of
    /// the package `pkgbase` uploaded by alice.
    async fn upload(
        &self,
        pkgbase: &str,
        platform: Option<Platform>,
        files: &[Pkg<'_>],
    ) -> Result<aurcache_common::api::package::UploadedBuild, UploadError> {
        self.upload_files(pkgbase, platform, files.iter().map(File::from).collect())
            .await
    }

    /// [`Env::upload`], of files as they are.
    async fn upload_files(
        &self,
        pkgbase: &str,
        platform: Option<Platform>,
        files: Vec<File>,
    ) -> Result<aurcache_common::api::package::UploadedBuild, UploadError> {
        let mut received = self.received(pkgbase).await?;
        for file in files {
            let incoming = received.accept(&file.name)?;
            std::fs::write(incoming.path, file.bytes).unwrap();
        }
        self.publish(pkgbase, platform, received).await
    }

    /// Start receiving an upload to `pkgbase`, which need not exist, as the
    /// API does: [`package_for`] first.
    async fn received(&self, pkgbase: &str) -> Result<Received, UploadError> {
        let pkg = aurcache_utils::upload::package_for(&self.db, pkgbase).await?;
        Ok(Received::new(&self.repo, pkgbase, pkg.as_ref(), 1 << 20)
            .await
            .unwrap())
    }

    /// Publish what `received` holds as a build of `pkgbase`.
    async fn publish(
        &self,
        pkgbase: &str,
        platform: Option<Platform>,
        received: Received,
    ) -> Result<aurcache_common::api::package::UploadedBuild, UploadError> {
        publish_upload(
            &self.db,
            &self.repo,
            &ActivityLog::discarding(),
            UploadRequest {
                pkgbase: pkgbase.to_string(),
                platform,
                uploader: Some("alice".to_string()),
            },
            received,
        )
        .await
    }

    async fn package(&self, name: &str) -> Option<packages::Model> {
        Packages::find()
            .filter(packages::Column::Name.eq(name))
            .one(&self.db)
            .await
            .unwrap()
    }

    async fn packages_named(&self, name: &str) -> Vec<packages::Model> {
        Packages::find()
            .filter(packages::Column::Name.eq(name))
            .all(&self.db)
            .await
            .unwrap()
    }

    async fn builds(&self) -> Vec<builds::Model> {
        Builds::find().all(&self.db).await.unwrap()
    }

    async fn filenames(&self) -> Vec<String> {
        let mut names: Vec<String> = Files::find()
            .all(&self.db)
            .await
            .unwrap()
            .into_iter()
            .map(|file| file.filename)
            .collect();
        names.sort();
        names
    }

    /// The entry directories `repo.db` lists for `platform`.
    fn listed(&self, platform: &str) -> Vec<String> {
        let Ok(mut file) =
            std::fs::File::open(self.repo.root().join(platform).join("repo.db.tar.gz"))
        else {
            return Vec::new();
        };
        let mut data = Vec::new();
        file.read_to_end(&mut data).unwrap();
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

    /// Whatever is left under the staging root: held uploads, build staging.
    ///
    /// A refused upload's files are removed off the async runtime, so this
    /// gives that a moment before answering.
    async fn staged(&self) -> Vec<String> {
        let read = || {
            std::fs::read_dir(self.repo.staging_root())
                .map(|entries| {
                    entries
                        .flatten()
                        .map(|e| e.file_name().to_string_lossy().into_owned())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        };
        for _ in 0..100 {
            let staged = read();
            if staged.is_empty() {
                return staged;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        read()
    }

    fn services(&self) -> Services {
        Services {
            db: self.db.clone(),
            store: Arc::new(SnapshotStore::with_checkout_root(
                tempfile::tempdir().unwrap().keep(),
            )),
            client: Arc::new(AurClient::with_urls("http://127.0.0.1:9")),
            repo: Arc::clone(&self.repo),
            activity: ActivityLog::discarding(),
        }
    }
}

fn refused(result: Result<impl std::fmt::Debug, UploadError>) -> String {
    match result {
        Err(UploadError::Refused(reason)) => reason,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

/// An upload to an AUR package whose split packages are `names`.
async fn aur_package(env: &Env, name: &str, names: &[&str]) -> packages::Model {
    let names: Vec<String> = names.iter().map(|n| (*n).to_string()).collect();
    package(name)
        .with(|row| row.split_packages = Set(Some(JsonList::new(names))))
        .insert(&env.db)
        .await
}

#[tokio::test]
async fn an_upload_is_published_as_a_build_of_the_package() {
    let env = env().await;
    aur_package(&env, "fresh", &["fresh", "fresh-docs"]).await;
    let uploaded = env
        .upload(
            "fresh",
            None,
            &[
                pkg("fresh", "fresh", "1.0-1", "x86_64"),
                pkg("fresh-docs", "fresh", "1.0-1", "any"),
            ],
        )
        .await
        .unwrap();

    assert_eq!(uploaded.status, BuildState::Successful);
    assert_eq!(uploaded.platform, "x86_64");
    assert_eq!(uploaded.version, "1.0-1");

    let pkg = env.package("fresh").await.unwrap();
    assert_eq!(pkg.status, BuildState::Successful);
    assert_eq!(
        JsonList::entries(pkg.split_packages.as_ref()),
        ["fresh", "fresh-docs"],
        "its names are its source's"
    );

    let builds = env.builds().await;
    assert_eq!(builds.len(), 1);
    assert_eq!(builds[0].number, uploaded.number);
    assert_eq!(builds[0].trigger, BuildTrigger::Upload);
    assert_eq!(builds[0].worker_id, None);
    assert_eq!(builds[0].version, "1.0-1");

    assert_eq!(
        env.filenames().await,
        [
            "fresh-1.0-1-x86_64.pkg.tar.zst",
            "fresh-docs-1.0-1-any.pkg.tar.zst"
        ]
    );
    assert_eq!(env.listed("x86_64"), ["fresh-1.0-1", "fresh-docs-1.0-1"]);
    assert!(env.staged().await.is_empty(), "nothing is left staged");

    let log = std::fs::read_to_string(aurcache_common::fs::build_log_path(
        "fresh",
        uploaded.number,
    ))
    .unwrap();
    assert!(log.starts_with("Uploaded by alice:\n"), "{log}");
    assert!(log.contains("fresh-docs-1.0-1-any.pkg.tar.zst"), "{log}");
}

/// A detached signature goes into the repository beside its package, and a
/// signature of nothing in the upload is refused.
#[tokio::test]
async fn a_signature_travels_with_its_package() {
    let env = env().await;
    aur_package(&env, "signed", &["signed", "signed-docs"]).await;
    let signed = pkg("signed", "signed", "1.0-1", "x86_64");
    let signature = signature();

    let mut stray = env.received("signed").await.unwrap();
    let incoming = stray.accept(&signed.filename()).unwrap();
    std::fs::write(incoming.path, signed.bytes()).unwrap();
    let incoming = stray
        .accept("signed-docs-1.0-1-x86_64.pkg.tar.zst.sig")
        .unwrap();
    std::fs::write(incoming.path, &signature).unwrap();
    let result = env.publish("signed", None, stray).await;
    assert!(refused(result).contains("signed-docs-1.0-1"));

    let mut received = env.received("signed").await.unwrap();
    let incoming = received.accept(&signed.filename()).unwrap();
    assert_eq!(incoming.limit, 1 << 20);
    std::fs::write(incoming.path, signed.bytes()).unwrap();
    let incoming = received
        .accept(&format!("{}.sig", signed.filename()))
        .unwrap();
    assert_eq!(incoming.limit, aurcache_utils::upload::MAX_SIGNATURE_BYTES);
    std::fs::write(incoming.path, &signature).unwrap();
    let uploaded = env.publish("signed", None, received).await.unwrap();

    assert_eq!(uploaded.status, BuildState::Successful);
    let published = env
        .repo
        .root()
        .join("x86_64")
        .join(format!("{}.sig", signed.filename()));
    assert_eq!(std::fs::read(published).unwrap(), signature);
}

/// A `.sig` that publishing could not carry, an armored one, fails the
/// build when publishing reads it, as a worker's would, and publishes
/// nothing.
#[tokio::test]
async fn an_armored_signature_fails_the_build() {
    let env = env().await;
    package("badsig").insert(&env.db).await;
    let signed = pkg("badsig", "badsig", "1.0-1", "x86_64");
    let uploaded = env
        .upload_files(
            "badsig",
            None,
            vec![
                File::from(&signed),
                File {
                    name: format!("{}.sig", signed.filename()),
                    bytes: b"-----BEGIN PGP SIGNATURE-----\n\niHUEABYKAB0WIQQg\n-----END PGP SIGNATURE-----\n"
                        .to_vec(),
                },
            ],
        )
        .await
        .unwrap();
    assert_eq!(uploaded.status, BuildState::Failed);
    let log = std::fs::read_to_string(aurcache_common::fs::build_log_path(
        "badsig",
        uploaded.number,
    ))
    .unwrap();
    assert!(log.contains("armored"), "{log}");
    assert!(env.listed("x86_64").is_empty());
    assert!(env.staged().await.is_empty(), "nothing is left staged");
}

/// Every package an upload carries is one its source builds, at a version
/// as makepkg writes one: anything else is refused as it arrives, before any
/// of it is held.
#[tokio::test]
async fn an_upload_takes_only_what_its_source_builds_as_it_arrives() {
    let env = env().await;
    package("fine").insert(&env.db).await;
    let mut received = env.received("fine").await.unwrap();
    received.accept("fine-1.0-1-x86_64.pkg.tar.zst").unwrap();
    for name in [
        "-rf-1.0-1-x86_64.pkg.tar.zst",
        "my pkg-1.0-1-x86_64.pkg.tar.zst",
        "openssh-1.0-1-x86_64.pkg.tar.zst",
        "openssh-1.0-1-x86_64.pkg.tar.zst.sig",
        "fine-1 0-1-x86_64.pkg.tar.zst",
        "fine-1.0-x-x86_64.pkg.tar.zst",
        "fine-1.0-1-x86_64.pkg.tar.zst",
    ] {
        match received.accept(name) {
            Err(UploadError::Refused(reason)) => assert!(reason.contains(name), "{reason}"),
            Err(other) => panic!("{name:?}: expected a refusal, got {other:?}"),
            Ok(_) => panic!("{name:?} was accepted"),
        }
    }
    let files: Vec<&str> = received
        .files()
        .iter()
        .map(aurcache_common::upload::File::name)
        .collect();
    assert_eq!(files, ["fine-1.0-1-x86_64.pkg.tar.zst"]);
}

/// A package that expands too far to be read fails its build, cleanly and
/// for good: nothing is left for a restart to resume and fail again.
#[tokio::test]
async fn a_package_too_large_to_read_fails_its_build_for_good() {
    let env = env().await;
    package("expands").insert(&env.db).await;
    let expands = pkg("expands", "expands", "1.0-1", "x86_64");
    let uploaded = env
        .upload_files(
            "expands",
            None,
            vec![File {
                name: expands.filename(),
                bytes: expands.expanding_bytes(80 << 20),
            }],
        )
        .await
        .unwrap();

    assert_eq!(uploaded.status, BuildState::Failed);
    let builds = env.builds().await;
    assert_eq!(builds.len(), 1);
    assert_eq!(builds[0].status, BuildState::Failed);
    assert!(
        aurcache_utils::publish::interrupted(&env.db, &env.repo)
            .await
            .unwrap()
            .is_empty(),
        "a restart has nothing to resume"
    );
    assert!(env.staged().await.is_empty(), "nothing is left staged");
    assert!(env.listed("x86_64").is_empty());
    let log = std::fs::read_to_string(aurcache_common::fs::build_log_path(
        "expands",
        uploaded.number,
    ))
    .unwrap();
    assert!(log.contains("decompresses to more than"), "{log}");
}

/// Files that cannot be moved to their build's staging directory fail the
/// build, even when failing it has to be retried: a build left publishing
/// would keep every later upload of the package on that platform out.
#[tokio::test]
async fn an_upload_that_cannot_be_staged_fails_its_build() {
    use sea_orm::ConnectionTrait;

    let env = env().await;
    aur_package(&env, "unstaged", &["unstaged"]).await;
    let mut received = env.received("unstaged").await.unwrap();
    let file = File::from(&pkg("unstaged", "unstaged", "1.0-1", "x86_64"));
    let incoming = received.accept(&file.name).unwrap();
    std::fs::write(incoming.path, file.bytes).unwrap();
    // The build's staging directory cannot be made: a file has its name.
    std::fs::write(env.repo.staging_dir(1), b"").unwrap();
    env.db
        .execute_unprepared(&format!(
            "CREATE TRIGGER reject_failing BEFORE UPDATE OF status ON builds \
             WHEN NEW.status = {} BEGIN SELECT RAISE(ABORT, 'failing it failed'); END",
            BuildState::Failed.as_i32()
        ))
        .await
        .unwrap();

    let result = env.publish("unstaged", None, received).await;
    assert!(
        matches!(result, Err(UploadError::Internal(_))),
        "{result:?}"
    );
    let builds = env.builds().await;
    assert_eq!(builds.len(), 1);
    assert_eq!(builds[0].id, 1);
    assert_eq!(builds[0].status, BuildState::Publishing);

    env.db
        .execute_unprepared("DROP TRIGGER reject_failing")
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        while env.builds().await[0].status != BuildState::Failed {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the build is failed in the background");
    let log = std::fs::read_to_string(aurcache_common::fs::build_log_path(
        "unstaged",
        builds[0].number,
    ))
    .unwrap();
    assert!(
        log.contains("Could not stage the uploaded files"),
        "the reason is recorded with the build: {log}"
    );
}

/// An upload is held to the names the package's source builds, as a worker's
/// build is.
#[tokio::test]
async fn an_upload_is_held_to_the_names_its_source_builds() {
    let env = env().await;
    aur_package(&env, "mixed", &["mixed", "mixed-docs"]).await;
    package("openssh").insert(&env.db).await;
    let reason = refused(
        env.upload(
            "mixed",
            None,
            &[
                pkg("mixed", "mixed", "1.0-1", "x86_64"),
                pkg("openssh", "openssh", "1.0-1", "x86_64"),
            ],
        )
        .await,
    );
    assert!(reason.contains("openssh"), "{reason}");
    assert!(
        reason.contains("mixed-docs"),
        "it says what may be: {reason}"
    );

    assert!(env.builds().await.is_empty());
    assert!(env.staged().await.is_empty(), "the held files are gone");
}

#[tokio::test]
async fn an_upload_for_a_platform_the_package_is_not_built_for_is_refused() {
    let env = env().await;
    // An AUR package configured for x86_64 only.
    package("narrow").insert(&env.db).await;

    let reason = refused(
        env.upload(
            "narrow",
            None,
            &[pkg("narrow", "narrow", "1.0-1", "aarch64")],
        )
        .await,
    );
    assert!(reason.contains("aarch64"), "{reason}");

    assert!(env.builds().await.is_empty());

    // A file that says one arch in its name and another inside is taken by
    // its name, and fails its build when publishing reads it, as a worker's
    // would.
    let mut received = env.received("narrow").await.unwrap();
    let incoming = received.accept("narrow-1.0-1-x86_64.pkg.tar.zst").unwrap();
    std::fs::write(
        incoming.path,
        pkg("narrow", "narrow", "1.0-1", "aarch64").bytes(),
    )
    .unwrap();
    let mismatched = env.publish("narrow", None, received).await.unwrap();
    assert_eq!(mismatched.status, BuildState::Failed);
    let log = std::fs::read_to_string(aurcache_common::fs::build_log_path(
        "narrow",
        mismatched.number,
    ))
    .unwrap();
    assert!(log.contains("holds arch 'aarch64'"), "{log}");
    assert!(env.listed("x86_64").is_empty());
}

#[tokio::test]
async fn a_new_upload_retires_the_previous_version() {
    let env = env().await;
    package("versioned").insert(&env.db).await;
    env.upload(
        "versioned",
        None,
        &[pkg("versioned", "versioned", "1.0-1", "x86_64")],
    )
    .await
    .unwrap();
    let second = env
        .upload(
            "versioned",
            None,
            &[pkg("versioned", "versioned", "1.1-1", "x86_64")],
        )
        .await
        .unwrap();

    assert_eq!(second.number, 2);
    assert_eq!(second.status, BuildState::Successful);
    assert_eq!(
        env.filenames().await,
        ["versioned-1.1-1-x86_64.pkg.tar.zst"]
    );
    assert_eq!(env.listed("x86_64"), ["versioned-1.1-1"]);
}

/// The issue's case: the package's own build fails where it is built, so it
/// is built elsewhere and uploaded.
#[tokio::test]
async fn an_upload_replaces_a_failed_build_of_an_aur_package() {
    let env = env().await;
    let oom = aur_package(&env, "oom", &["oom", "oom-docs"]).await;
    let failed = build(&oom).status(BuildState::Failed).insert(&env.db).await;
    sea_orm::ActiveModelTrait::update(
        packages::ActiveModel {
            id: Set(oom.id),
            status: Set(BuildState::Failed),
            ..Default::default()
        },
        &env.db,
    )
    .await
    .unwrap();

    let uploaded = env
        .upload("oom", None, &[pkg("oom", "oom", "1.0-1", "x86_64")])
        .await
        .unwrap();
    assert_eq!(uploaded.status, BuildState::Successful);
    assert_eq!(uploaded.number, failed.number + 1);
    let oom = env.package("oom").await.unwrap();
    assert_eq!(oom.status, BuildState::Successful);
    assert_eq!(env.listed("x86_64"), ["oom-1.0-1"]);
}

#[tokio::test]
async fn an_upload_releases_the_builds_waiting_on_it() {
    let env = env().await;
    let dep = package("dep").insert(&env.db).await;
    env.upload("dep", None, &[pkg("dep", "dep", "1.0-1", "x86_64")])
        .await
        .unwrap();
    let parent = package("parent").insert(&env.db).await;
    depend(&env.db, &parent, &dep, ">=2.0").await;
    let waiting = build(&parent)
        .status(BuildState::WaitingForDeps)
        .with(|row| row.end_time = Set(None))
        .insert(&env.db)
        .await;

    env.upload("dep", None, &[pkg("dep", "dep", "2.0-1", "x86_64")])
        .await
        .unwrap();

    let promoted = Builds::find_by_id(waiting.id)
        .one(&env.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(promoted.status, BuildState::Enqueued);
}

#[tokio::test]
async fn an_upload_does_not_overtake_a_build_in_progress() {
    let env = env().await;
    let busy = package("busy").insert(&env.db).await;
    build(&busy)
        .status(BuildState::Enqueued)
        .with(|row| row.end_time = Set(None))
        .insert(&env.db)
        .await;

    let result = env
        .upload("busy", None, &[pkg("busy", "busy", "1.0-1", "x86_64")])
        .await;
    assert!(matches!(result, Err(UploadError::Busy(_))), "{result:?}");
    assert_eq!(env.builds().await.len(), 1);
    assert!(env.staged().await.is_empty());
}

#[tokio::test]
async fn an_any_only_upload_goes_to_the_named_platform() {
    let env = env().await;
    package("notes")
        .platforms("x86_64;aarch64")
        .insert(&env.db)
        .await;
    let reason = refused(
        env.upload("notes", None, &[pkg("notes", "notes", "1.0-1", "any")])
            .await,
    );
    assert!(reason.contains("platform"), "{reason}");

    let uploaded = env
        .upload(
            "notes",
            Some(Platform::Aarch64),
            &[pkg("notes", "notes", "1.0-1", "any")],
        )
        .await
        .unwrap();
    assert_eq!(uploaded.platform, "aarch64");
    assert_eq!(env.listed("aarch64"), ["notes-1.0-1"]);
}

/// makechrootpkg writes a `-debug` package beside the real ones by default:
/// it is left out, with its signature, and the rest is published.
#[tokio::test]
async fn a_debug_package_is_left_out() {
    let env = env().await;
    package("dbg").insert(&env.db).await;
    let debug = pkg("dbg-debug", "dbg", "1.0-1", "x86_64");
    let uploaded = env
        .upload_files(
            "dbg",
            None,
            vec![
                File::from(&pkg("dbg", "dbg", "1.0-1", "x86_64")),
                File::from(&debug),
                File {
                    name: format!("{}.sig", debug.filename()),
                    bytes: vec![0x89; 64],
                },
            ],
        )
        .await
        .unwrap();

    assert_eq!(uploaded.status, BuildState::Successful);
    assert_eq!(env.listed("x86_64"), ["dbg-1.0-1"]);
    assert_eq!(env.filenames().await, ["dbg-1.0-1-x86_64.pkg.tar.zst"]);
    let log = std::fs::read_to_string(aurcache_common::fs::build_log_path("dbg", uploaded.number))
        .unwrap();
    assert!(log.contains("skipping debug package"), "{log}");
}

/// A package tracked under a name that is not its pkgbase takes uploads of
/// what its source builds.
#[tokio::test]
async fn a_package_tracked_under_another_name_takes_its_own_pkgbase() {
    let env = env().await;
    aur_package(&env, "tracked", &["real"]).await;
    let uploaded = env
        .upload("tracked", None, &[pkg("real", "real", "1.0-1", "x86_64")])
        .await
        .unwrap();
    assert_eq!(uploaded.status, BuildState::Successful);
    assert_eq!(env.listed("x86_64"), ["real-1.0-1"]);
}

/// An upload a restart interrupted is removed at startup; a build's staging
/// directory beside it is left for publishing to resume.
#[tokio::test]
async fn an_interrupted_upload_is_discarded_at_startup() {
    let env = env().await;
    package("cut").insert(&env.db).await;
    let mut received = env.received("cut").await.unwrap();
    let incoming = received.accept("cut-1.0-1-x86_64.pkg.tar.zst").unwrap();
    std::fs::write(&incoming.path, b"half a package").unwrap();
    // What a process that died mid-upload leaves: the holding directory,
    // never removed.
    let held = received.path().to_path_buf();
    std::mem::forget(received);
    let build_staging = env.repo.staging_dir(7);
    std::fs::create_dir_all(&build_staging).unwrap();

    assert_eq!(
        aurcache_utils::upload::discard_abandoned(&env.repo).await,
        1
    );
    assert!(!held.exists());
    assert!(
        build_staging.exists(),
        "a build's staging is not an upload's"
    );
}

/// A package removed while its upload was being checked is told apart from
/// a failure of the server's own, and nothing is recorded.
#[tokio::test]
async fn an_upload_to_a_package_removed_meanwhile_is_contended() {
    let env = env().await;
    package("gone").insert(&env.db).await;
    let gone = pkg("gone", "gone", "1.0-1", "x86_64");
    let mut received = env.received("gone").await.unwrap();
    let incoming = received.accept(&gone.filename()).unwrap();
    std::fs::write(incoming.path, gone.bytes()).unwrap();
    let pkg = env.package("gone").await.unwrap();
    Packages::delete_by_id(pkg.id).exec(&env.db).await.unwrap();

    let result = publish_upload(
        &env.db,
        &env.repo,
        &ActivityLog::discarding(),
        UploadRequest {
            pkgbase: "gone".to_string(),
            platform: None,
            uploader: None,
        },
        received,
    )
    .await;
    assert!(
        matches!(&result, Err(UploadError::Contended(reason)) if reason.contains("removed")),
        "{result:?}"
    );
    assert!(env.builds().await.is_empty());
    assert!(
        env.packages_named("gone").await.is_empty(),
        "nor re-created"
    );
    assert!(env.staged().await.is_empty(), "the held files are gone");
}

/// A restart between recording an upload's build and staging its files
/// finds a `PUBLISHING` build with nothing staged: publishing fails it,
/// rather than publishing nothing, and there is nothing left to resume.
#[tokio::test]
async fn an_upload_recorded_but_never_staged_fails_its_build() {
    let env = env().await;
    let pkg = package("crashed").insert(&env.db).await;
    let aurcache_db::helpers::build_enqueue::UploadSlot::Recorded(build) =
        aurcache_db::helpers::build_enqueue::record_upload(
            &env.db,
            pkg.id,
            Platform::X86_64,
            "1.0-1",
            aurcache_db::helpers::time::now_secs(),
        )
        .await
        .unwrap()
    else {
        panic!("the upload is recorded");
    };

    let resumed = aurcache_utils::publish::interrupted(&env.db, &env.repo)
        .await
        .unwrap();
    assert_eq!(resumed, [build.id]);
    aurcache_utils::publish::publish_build(
        &env.db,
        &env.repo,
        &ActivityLog::discarding(),
        build.id,
    )
    .await;

    let failed = Builds::find_by_id(build.id)
        .one(&env.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(failed.status, BuildState::Failed);
    assert_eq!(
        env.package("crashed").await.unwrap().status,
        BuildState::Failed
    );
    assert!(
        aurcache_utils::publish::interrupted(&env.db, &env.repo)
            .await
            .unwrap()
            .is_empty()
    );
    let log = std::fs::read_to_string(aurcache_common::fs::build_log_path("crashed", build.number))
        .unwrap();
    assert!(log.contains("no artifacts were uploaded"), "{log}");
}

/// A new package's name is checked before any of its files arrive.
#[tokio::test]
async fn a_new_pkgbase_is_checked_before_the_upload_arrives() {
    use aurcache_utils::upload::package_for;
    let env = env().await;
    let two = package("two").insert(&env.db).await;
    assert_eq!(
        package_for(&env.db, "two").await.unwrap().map(|pkg| pkg.id),
        Some(two.id)
    );
    assert!(package_for(&env.db, "new").await.unwrap().is_none());
    let reason = refused(package_for(&env.db, "New").await);
    assert!(reason.contains("not a package name"), "{reason}");
}

#[tokio::test]
async fn an_upload_creates_an_uploaded_package_and_publishes_it() {
    let env = env().await;
    let uploaded = env
        .upload(
            "fresh",
            None,
            &[
                pkg("fresh", "fresh", "1.0-1", "x86_64"),
                pkg("fresh-docs", "fresh", "1.0-1", "any"),
            ],
        )
        .await
        .unwrap();

    assert!(uploaded.created);
    assert_eq!(uploaded.status, BuildState::Successful);
    assert_eq!(uploaded.platform, "x86_64");
    assert_eq!(uploaded.version, "1.0-1");

    let pkg = env.package("fresh").await.unwrap();
    assert_eq!(pkg.source_data, SourceData::Upload {});
    assert_eq!(pkg.status, BuildState::Successful);
    assert!(pkg.platforms.contains(Platform::X86_64));
    assert_eq!(
        JsonList::entries(pkg.split_packages.as_ref()),
        ["fresh", "fresh-docs"]
    );
    assert_eq!(
        JsonList::entries(pkg.provides.as_ref()),
        ["fresh-virtual", "fresh-docs-virtual"]
    );
    assert_eq!(pkg.source_description.as_deref(), Some("fresh for tests"));

    let builds = env.builds().await;
    assert_eq!(builds.len(), 1);
    assert_eq!(builds[0].number, uploaded.number);
    assert_eq!(builds[0].trigger, BuildTrigger::Upload);
    assert_eq!(builds[0].worker_id, None);
    assert_eq!(builds[0].version, "1.0-1");

    assert_eq!(
        env.filenames().await,
        [
            "fresh-1.0-1-x86_64.pkg.tar.zst",
            "fresh-docs-1.0-1-any.pkg.tar.zst"
        ]
    );
    assert_eq!(env.listed("x86_64"), ["fresh-1.0-1", "fresh-docs-1.0-1"]);
    assert!(env.staged().await.is_empty(), "nothing is left staged");

    let log = std::fs::read_to_string(aurcache_common::fs::build_log_path(
        "fresh",
        uploaded.number,
    ))
    .unwrap();
    assert!(log.starts_with("Uploaded by alice:\n"), "{log}");
    assert!(log.contains("fresh-docs-1.0-1-any.pkg.tar.zst"), "{log}");
}

#[tokio::test]
async fn an_uploaded_package_is_never_queued_or_claimed() {
    let env = env().await;
    env.upload(
        "elsewhere",
        None,
        &[pkg("elsewhere", "elsewhere", "1.0-1", "x86_64")],
    )
    .await
    .unwrap();
    // A platform with no build yet: what the startup scan would fill.
    let pkg = env.package("elsewhere").await.unwrap();
    sea_orm::ActiveModelTrait::update(
        packages::ActiveModel {
            id: Set(pkg.id),
            platforms: Set("x86_64;aarch64".parse().unwrap()),
            ..Default::default()
        },
        &env.db,
    )
    .await
    .unwrap();

    assert_eq!(
        enqueue_missing_buildable_packages(&env.db, &ActivityLog::discarding())
            .await
            .unwrap(),
        0
    );
    let pkg = env.package("elsewhere").await.unwrap();
    assert!(
        package_update(&env.services(), pkg, Rebuild::Always, BuildTrigger::User)
            .await
            .is_err(),
        "a rebuild of an uploaded package is refused"
    );
    let builds = env.builds().await;
    assert_eq!(builds.len(), 1, "only the upload: {builds:?}");

    // A worker that would take anything finds nothing.
    env.db
        .execute_unprepared(&format!(
            "INSERT INTO workers (id, name, status, cert_fingerprint, arches, \
             package_affinity, priority, concurrency, last_seen, settings_declaration) \
             VALUES (1, 'w1', '{}', 'fp1', 'x86_64,aarch64', '', 0, 4, {}, '[]')",
            ApprovalStatus::Approved,
            aurcache_db::helpers::time::now_secs()
        ))
        .await
        .unwrap();
    let claimed = aurcache_db::helpers::worker_jobs::claim_job(&env.db, 1, 60, 0, 600)
        .await
        .unwrap();
    assert!(claimed.is_none(), "claimed {claimed:?}");

    // The same worker does take a package it can build: it was idle, not
    // unable.
    let other = package("other").insert(&env.db).await;
    build(&other)
        .status(BuildState::Enqueued)
        .with(|row| row.end_time = Set(None))
        .insert(&env.db)
        .await;
    let claimed = aurcache_db::helpers::worker_jobs::claim_job(&env.db, 1, 60, 0, 600)
        .await
        .unwrap();
    assert_eq!(claimed.map(|b| b.pkg_id), Some(other.id));
}

/// A name another package publishes is refused before anything is recorded:
/// `repo.db` would list two packages of one name.
#[tokio::test]
async fn a_name_another_package_publishes_is_refused() {
    let env = env().await;
    package("openssh").insert(&env.db).await;
    env.upload(
        "openssh",
        None,
        &[pkg("openssh", "openssh", "9.9-1", "x86_64")],
    )
    .await
    .unwrap();
    let reason = refused(
        env.upload(
            "claimer",
            None,
            &[
                pkg("claimer", "claimer", "1.0-1", "x86_64"),
                pkg("openssh", "claimer", "1.0-1", "x86_64"),
            ],
        )
        .await,
    );
    assert!(
        reason.contains("'openssh' is already published by the package 'openssh'"),
        "{reason}"
    );
    assert!(env.package("claimer").await.is_none());

    // A name another package only builds, and does not publish, is no one's
    // yet: whichever publishes it first has it.
    package("unpublished")
        .with(|row| row.split_packages = Set(Some(JsonList::new(["unpublished", "spare"]))))
        .insert(&env.db)
        .await;
    let spare = env
        .upload("spare", None, &[pkg("spare", "spare", "1.0-1", "x86_64")])
        .await
        .unwrap();
    assert_eq!(spare.status, BuildState::Successful);
}

/// An upload that is refused while publishing leaves the package's names as
/// they were: a name it still publishes cannot be taken by another package.
#[tokio::test]
async fn a_failed_upload_keeps_the_names_the_package_publishes() {
    let env = env().await;
    env.upload(
        "kept",
        None,
        &[
            pkg("kept", "kept", "1.0-1", "x86_64"),
            pkg("kept-docs", "kept", "1.0-1", "any"),
        ],
    )
    .await
    .unwrap();

    let newer = pkg("kept", "kept", "1.1-1", "x86_64");
    let failed = env
        .upload_files(
            "kept",
            None,
            vec![File {
                name: newer.filename(),
                bytes: newer.truncated_bytes(),
            }],
        )
        .await
        .unwrap();
    assert_eq!(failed.status, BuildState::Failed);
    let kept = env.package("kept").await.unwrap();
    assert_eq!(
        JsonList::entries(kept.split_packages.as_ref()),
        ["kept", "kept-docs"]
    );
    assert_eq!(env.listed("x86_64"), ["kept-1.0-1", "kept-docs-1.0-1"]);

    let reason = refused(
        env.upload(
            "thief",
            Some(Platform::X86_64),
            &[pkg("kept-docs", "thief", "2.0-1", "any")],
        )
        .await,
    );
    assert!(reason.contains("kept-docs"), "{reason}");

    // Once an upload without it is published, the name is free again.
    env.upload("kept", None, &[pkg("kept", "kept", "1.2-1", "x86_64")])
        .await
        .unwrap();
    assert_eq!(
        JsonList::entries(env.package("kept").await.unwrap().split_packages.as_ref()),
        [] as [String; 0]
    );
}

#[tokio::test]
async fn a_new_pkgbase_must_be_a_package_name() {
    let env = env().await;
    for name in ["a/b", "Upper", "-dash"] {
        let reason = refused(
            env.upload(name, None, &[pkg("x", name, "1.0-1", "x86_64")])
                .await,
        );
        assert!(reason.contains("not a package name"), "{reason}");
    }
    assert!(env.builds().await.is_empty());
}

/// Two first uploads of different packages publishing one name at once:
/// both pass the check made before they are recorded, and publishing, under
/// the repository lock, fails the second.
#[tokio::test]
async fn two_uploads_cannot_both_publish_a_name() {
    let env = env().await;
    let left_files = [pkg("shared", "left", "1.0-1", "x86_64")];
    let right_files = [pkg("shared", "right", "1.0-1", "x86_64")];
    let (left, right) = tokio::join!(
        env.upload("left", None, &left_files),
        env.upload("right", None, &right_files),
    );
    let published: Vec<BuildState> = [&left, &right]
        .into_iter()
        .filter_map(|result| result.as_ref().ok().map(|uploaded| uploaded.status))
        .collect();
    assert_eq!(
        published
            .iter()
            .filter(|status| **status == BuildState::Successful)
            .count(),
        1,
        "{left:?} / {right:?}"
    );
    assert_eq!(env.listed("x86_64"), ["shared-1.0-1"]);
}

/// An uploaded package with no build to go by -- its only one deleted, or
/// restored without its files -- reads as failed rather than queued: nothing
/// will build it.
#[tokio::test]
async fn an_uploaded_package_without_builds_reads_failed() {
    let env = env().await;
    let pkg = package("bare")
        .source(SourceData::Upload {})
        .status(BuildState::Enqueued)
        .insert(&env.db)
        .await;
    aurcache_db::helpers::builds::refresh_package_status(&env.db, pkg.id)
        .await
        .unwrap();
    assert_eq!(
        env.package("bare").await.unwrap().status,
        BuildState::Failed
    );
}

/// A name an AUR package's source no longer builds is still its own while it
/// publishes a file of that name: an uploaded package taking it would put two
/// packages of one name in the repository. The files say, not the names its
/// source builds.
#[tokio::test]
async fn a_name_another_package_still_publishes_is_refused() {
    let env = env().await;
    aur_package(&env, "keeper", &["keeper", "dropped"]).await;
    env.upload(
        "keeper",
        None,
        &[
            pkg("keeper", "keeper", "1.0-1", "x86_64"),
            pkg("dropped", "keeper", "1.0-1", "x86_64"),
        ],
    )
    .await
    .unwrap();
    // Its source drops `dropped`; no build without it has been published.
    let keeper = env.package("keeper").await.unwrap();
    sea_orm::ActiveModelTrait::update(
        packages::ActiveModel {
            id: Set(keeper.id),
            split_packages: Set(Some(JsonList::new(["keeper"]))),
            ..Default::default()
        },
        &env.db,
    )
    .await
    .unwrap();

    let reason = refused(
        env.upload(
            "dropped",
            None,
            &[pkg("dropped", "dropped", "2.0-1", "x86_64")],
        )
        .await,
    );
    assert!(
        reason.contains("'dropped' is already published by the package 'keeper'"),
        "{reason}"
    );
    assert!(
        env.package("dropped").await.is_none(),
        "nothing was created"
    );
    assert_eq!(env.listed("x86_64"), ["dropped-1.0-1", "keeper-1.0-1"]);
}
