//! Uploading packages built elsewhere as builds of existing packages: what is
//! accepted, and what it becomes. Publishing itself is `publish.rs`'s.

mod common;

use aurcache_activitylog::activity_utils::ActivityLog;
use aurcache_common::api::package::UploadedBuild;
use aurcache_common::build_state::{BuildState, BuildTrigger};
use aurcache_db::lists::JsonList;
use aurcache_db::prelude::{Builds, Files, Packages};
use aurcache_db::testing::{build, memory_db, package};
use aurcache_db::{builds, packages};
use aurcache_utils::repository::Repository;
use aurcache_utils::upload::{Received, UploadError, UploadRequest, publish_upload};
use common::{Pkg, listed, pkg, quiet_logs};
use pacman_mirrors::platforms::Platform;
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, Set};
use std::sync::Arc;

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

impl File {
    /// A file whose contents are never read.
    fn named(name: &str) -> Self {
        Self {
            name: name.to_string(),
            bytes: b"x".to_vec(),
        }
    }

    /// A binary detached signature of `pkg`, as far as publishing reads one.
    fn signature(pkg: &Pkg<'_>) -> Self {
        Self {
            name: format!("{}.sig", pkg.filename()),
            bytes: vec![0x89; 64],
        }
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
        files: Vec<File>,
    ) -> Result<UploadedBuild, UploadError> {
        let pkg = self.package(pkgbase).await;
        let mut received = Received::new(&self.repo, &pkg, 1 << 20).await.unwrap();
        for file in files {
            let incoming = received.accept(&file.name)?;
            std::fs::write(incoming.path, file.bytes).unwrap();
        }
        let request = UploadRequest {
            pkg,
            platform,
            uploader: Some("alice".to_string()),
        };
        publish_upload(
            &self.db,
            &self.repo,
            &ActivityLog::discarding(),
            request,
            received,
        )
        .await
    }

    async fn package(&self, name: &str) -> packages::Model {
        Packages::find()
            .filter(packages::Column::Name.eq(name))
            .one(&self.db)
            .await
            .unwrap()
            .expect("the package exists")
    }

    async fn builds(&self) -> Vec<builds::Model> {
        Builds::find().all(&self.db).await.unwrap()
    }

    fn listed(&self, platform: &str) -> Vec<String> {
        listed(self.repo.root(), platform)
    }

    /// Whatever is left under the staging root: held uploads, build staging.
    fn staged(&self) -> Vec<String> {
        std::fs::read_dir(self.repo.staging_root())
            .map(|entries| {
                entries
                    .flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn log(&self, pkgbase: &str, number: i32) -> String {
        std::fs::read_to_string(aurcache_common::fs::build_log_path(pkgbase, number)).unwrap()
    }
}

/// An AUR package whose split packages are `names`, for `platforms`.
async fn aur_package(env: &Env, name: &str, names: &[&str], platforms: &str) {
    let names: Vec<String> = names.iter().map(ToString::to_string).collect();
    package(name)
        .platforms(platforms)
        .with(|row| row.split_packages = Set(Some(JsonList::new(names))))
        .insert(&env.db)
        .await;
}

#[tokio::test]
async fn an_upload_is_published_as_a_build_of_the_package() {
    let env = env().await;
    aur_package(&env, "fresh", &["fresh", "fresh-docs"], "x86_64").await;
    let main = pkg("fresh", "1.0-1", "x86_64");
    let docs = pkg("fresh-docs", "1.0-1", "any");
    let files = vec![File::from(&main), File::signature(&main), File::from(&docs)];
    let uploaded = env.upload("fresh", None, files).await.unwrap();

    assert_eq!(uploaded.status, BuildState::Successful);
    assert_eq!(uploaded.platform, "x86_64");
    assert_eq!(uploaded.version, "1.0-1");
    assert_eq!(env.package("fresh").await.status, BuildState::Successful);

    let builds = env.builds().await;
    assert_eq!(builds.len(), 1);
    assert_eq!(builds[0].number, uploaded.number);
    assert_eq!(builds[0].trigger, BuildTrigger::Upload);
    assert_eq!(builds[0].worker_id, None);

    let mut files: Vec<String> = Files::find()
        .all(&env.db)
        .await
        .unwrap()
        .into_iter()
        .map(|file| file.filename)
        .collect();
    files.sort();
    assert_eq!(files, [main.filename(), docs.filename()]);
    assert_eq!(env.listed("x86_64"), ["fresh-1.0-1", "fresh-docs-1.0-1"]);
    let signature = env
        .repo
        .root()
        .join("x86_64")
        .join(format!("{}.sig", main.filename()));
    assert_eq!(
        std::fs::read(signature).unwrap(),
        File::signature(&main).bytes
    );
    assert!(env.staged().is_empty(), "nothing is left staged");

    let log = env.log("fresh", uploaded.number);
    assert!(log.starts_with("Uploaded by alice:\n"), "{log}");
    assert!(log.contains(&docs.filename()), "{log}");
}

/// What the files hold is publishing's to check: an armored `.sig` fails the
/// build, as a worker's would, and publishes nothing.
#[tokio::test]
async fn an_armored_signature_fails_the_build() {
    let env = env().await;
    package("badsig").insert(&env.db).await;
    let signed = pkg("badsig", "1.0-1", "x86_64");
    let armored = File {
        name: format!("{}.sig", signed.filename()),
        bytes: b"-----BEGIN PGP SIGNATURE-----\n-----END PGP SIGNATURE-----\n".to_vec(),
    };
    let uploaded = env
        .upload("badsig", None, vec![File::from(&signed), armored])
        .await
        .unwrap();

    assert_eq!(uploaded.status, BuildState::Failed);
    assert!(env.log("badsig", uploaded.number).contains("armored"));
    assert!(env.listed("x86_64").is_empty());
    assert!(env.staged().is_empty(), "nothing is left staged");
}

/// Every refusal records nothing and leaves nothing held. The package builds
/// `tbl` and `tbl-docs`, for x86_64 and aarch64.
#[tokio::test]
async fn a_refused_upload_records_nothing() {
    let env = env().await;
    aur_package(&env, "tbl", &["tbl", "tbl-docs"], "x86_64;aarch64").await;
    let x86 = "tbl-1.0-1-x86_64.pkg.tar.zst";
    let refuses = async |names: &[&str], platform: Option<Platform>, reason: &str| {
        let files = names.iter().map(|name| File::named(name)).collect();
        match env.upload("tbl", platform, files).await {
            Err(UploadError::Refused(why)) => assert!(why.contains(reason), "{names:?}: {why}"),
            other => panic!("{names:?}: expected a refusal, got {other:?}"),
        }
        assert!(env.builds().await.is_empty(), "{names:?}");
        assert!(env.staged().is_empty(), "{names:?}");
    };
    refuses(&["PKGBUILD"], None, "neither a package").await;
    refuses(&["-rf-1.0-1-x86_64.pkg.tar.zst"], None, "not among").await;
    refuses(&["my pkg-1.0-1-x86_64.pkg.tar.zst"], None, "not among").await;
    let openssh = "openssh-1.0-1-x86_64.pkg.tar.zst";
    refuses(&[x86, openssh], None, "tbl, tbl-docs").await;
    refuses(&[x86, &format!("{openssh}.sig")], None, "not among").await;
    // Each package once, refused as it arrives: a second version is never
    // written.
    refuses(&[x86, x86], None, "already carries a version").await;
    let newer = "tbl-1.1-1-x86_64.pkg.tar.zst";
    refuses(&[x86, newer], None, "already carries a version").await;
    let stray = "tbl-docs-1.0-1-x86_64.pkg.tar.zst.sig";
    refuses(&[x86, stray], None, "not in the upload").await;
    let docs = "tbl-docs-1.1-1-x86_64.pkg.tar.zst";
    refuses(&[x86, docs], None, "version 1.1-1").await;
    let docs = "tbl-docs-1.0-1-aarch64.pkg.tar.zst";
    refuses(&[x86, docs], None, "for aarch64").await;
    let armv7h = "tbl-1.0-1-armv7h.pkg.tar.zst";
    refuses(&[armv7h], None, "not configured for armv7h").await;
    let riscv64 = "tbl-1.0-1-riscv64.pkg.tar.zst";
    refuses(&[riscv64], None, "not served here").await;
    refuses(&["tbl-1.0-1-any.pkg.tar.zst"], None, "--platform").await;
    refuses(&[x86], Some(Platform::Aarch64), "not aarch64").await;
}

/// An upload of only `arch = any` packages goes to the platform it names,
/// or to the package's only platform.
#[tokio::test]
async fn an_any_only_upload_goes_to_the_named_platform() {
    let env = env().await;
    package("notes")
        .platforms("x86_64;aarch64")
        .insert(&env.db)
        .await;
    let notes = pkg("notes", "1.0-1", "any");
    let uploaded = env
        .upload("notes", Some(Platform::Aarch64), vec![File::from(&notes)])
        .await
        .unwrap();
    assert_eq!(uploaded.platform, "aarch64");
    assert_eq!(env.listed("aarch64"), ["notes-1.0-1"]);

    package("memo").platforms("aarch64").insert(&env.db).await;
    let memo = File::from(&pkg("memo", "1.0-1", "any"));
    let uploaded = env.upload("memo", None, vec![memo]).await.unwrap();
    assert_eq!(uploaded.platform, "aarch64");
}

/// Files that cannot be moved to their build's staging directory fail the
/// build, with the reason in its log.
#[tokio::test]
async fn an_upload_that_cannot_be_staged_fails_its_build() {
    let env = env().await;
    package("unstaged").insert(&env.db).await;
    // The build's staging directory cannot be made: a file has its name.
    std::fs::create_dir_all(env.repo.staging_root()).unwrap();
    std::fs::write(env.repo.staging_dir(1), b"").unwrap();
    let files = vec![File::from(&pkg("unstaged", "1.0-1", "x86_64"))];
    let uploaded = env.upload("unstaged", None, files).await.unwrap();

    assert_eq!(uploaded.status, BuildState::Failed);
    let log = env.log("unstaged", uploaded.number);
    assert!(log.contains("Could not stage the uploaded files"), "{log}");
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

    let files = vec![File::from(&pkg("busy", "1.0-1", "x86_64"))];
    let result = env.upload("busy", None, files).await;
    assert!(matches!(result, Err(UploadError::Busy(_))), "{result:?}");
    assert_eq!(env.builds().await.len(), 1);
    assert!(env.staged().is_empty());
}

/// An upload a restart interrupted is removed at startup; a build's staging
/// directory beside it is left for publishing to resume.
#[tokio::test]
async fn an_interrupted_upload_is_discarded_at_startup() {
    let env = env().await;
    package("cut").insert(&env.db).await;
    let mut received = Received::new(&env.repo, &env.package("cut").await, 1 << 20)
        .await
        .unwrap();
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

/// A restart between recording an upload's build and staging its files
/// finds a `PUBLISHING` build with nothing staged: publishing fails it,
/// rather than publishing nothing, and there is nothing left to resume.
#[tokio::test]
async fn an_upload_recorded_but_never_staged_fails_its_build() {
    let env = env().await;
    let pkg = package("crashed").insert(&env.db).await;
    let build = aurcache_db::helpers::build_enqueue::enqueue_build_if_missing(
        &env.db,
        pkg.id,
        Platform::X86_64,
        "1.0-1",
        aurcache_db::helpers::time::now_secs(),
        aurcache_db::helpers::build_enqueue::Pending::Publishing,
        BuildTrigger::Upload,
    )
    .await
    .unwrap()
    .build;
    let interrupted = || aurcache_utils::publish::interrupted(&env.db, &env.repo);

    assert_eq!(interrupted().await.unwrap(), [build.id]);
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
    assert_eq!(env.package("crashed").await.status, BuildState::Failed);
    assert!(interrupted().await.unwrap().is_empty());
    let log = env.log("crashed", build.number);
    assert!(log.contains("no artifacts were uploaded"), "{log}");
}
