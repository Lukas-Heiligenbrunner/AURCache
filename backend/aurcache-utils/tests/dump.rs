//! What a dump carries, and what it deliberately leaves behind.

use aurcache_activitylog::activity_utils::ActivityLog;
use aurcache_common::api::dump::{DUMP_SCHEMA_VERSION, MANIFEST_FILE, PACKAGES_FILE};
use aurcache_common::build_state::BuildState;
use aurcache_common::source::GitSourceSpec;
use aurcache_db::packages::SourceData;
use aurcache_db::prelude::Packages;
use aurcache_db::{packages, settings, workers};
use aurcache_utils::dump::{SecretSources, build_dump, write_archive};
use aurcache_utils::services::Services;
use aurcache_utils::snapshot::SnapshotStore;
use aurcache_utils::ssh::{HostKeySource, KeySource, SshFiles};
use flate2::read::GzDecoder;
use sea_orm::{ActiveModelTrait, DatabaseConnection, EntityTrait, Set};
use std::collections::HashMap;
use std::io::Read;
use std::sync::Arc;

/// A dump's secrets read from `dir`: the CA, the signing key and the server's
/// own SSH files.
fn secrets_in(dir: &std::path::Path) -> SecretSources {
    SecretSources {
        ca_dir: dir.to_path_buf(),
        ssh: SshFiles::own(dir),
    }
}

/// The SSH files in `dir`, supplied by the operator.
fn supplied_ssh(dir: &std::path::Path) -> SshFiles {
    SshFiles {
        key: KeySource::Provided(dir.join("id_ed25519")),
        known_hosts: HostKeySource::Pinned(dir.join("known_hosts")),
    }
}

/// A repository of its own for a restore that publishes nothing new.
fn test_repo() -> aurcache_utils::repository::Repository {
    aurcache_utils::repository::Repository::new(tempfile::tempdir().unwrap().keep())
}

/// A store whose AUR is a local directory holding `hello`, so that a restore
/// reads that package's source without the network.
fn store_with_hello() -> Arc<SnapshotStore> {
    let aur = tempfile::tempdir().unwrap().keep();
    git_package(&aur.join("hello.git"), "hello", &[]);
    Arc::new(SnapshotStore::with_checkout_root_and_aur_base(
        tempfile::tempdir().unwrap().keep(),
        aur.to_string_lossy(),
    ))
}

async fn package(
    db: &DatabaseConnection,
    name: &str,
    directly_requested: bool,
    patch: Option<&str>,
) -> i32 {
    packages::ActiveModel {
        name: Set(name.to_string()),
        // Derived state, deliberately set to something a restore must not
        // carry over: the dump should describe the package, not its history.
        status: Set(BuildState::Successful),
        out_of_date: Set(true),
        upstream_version: Set(Some("9.9.9".to_string())),
        build_flags: Set("--noconfirm;;--nocolor".parse().unwrap()),
        platforms: Set("x86_64;aarch64".parse().unwrap()),
        source_data: Set(SourceData::Aur {
            name: name.to_string(),
        }),
        directly_requested: Set(directly_requested),
        split_packages: Set(Some(r#"["a","b"]"#.parse().unwrap())),
        patch: Set(patch.map(str::to_string)),
        ..Default::default()
    }
    .insert(db)
    .await
    .unwrap()
    .id
}

/// A client, and the directory its official repositories are read from,
/// which goes when this does.
struct TestClient {
    client: aurcache_deps::AurClient,
    _repos: tempfile::TempDir,
}

/// The headline: authored state in, derived state out.
/// An AUR client whose official repositories are read and hold nothing.
///
/// Restore resolves sources, and resolution asks the repositories first, so a
/// client that has never read them refuses to answer. Present-and-empty is how
/// a test says "the repositories hold nothing" -- absent would mean "could not
/// ask", which is a different answer and deliberately fatal.
async fn client_with_empty_official_repos() -> TestClient {
    let dir = tempfile::tempdir().unwrap();
    for repo in ["core", "extra", "multilib"] {
        let mut archive = Vec::new();
        {
            let encoder =
                flate2::write::GzEncoder::new(&mut archive, flate2::Compression::default());
            let mut builder = tar::Builder::new(encoder);
            builder.finish().unwrap();
            builder.into_inner().unwrap().finish().unwrap();
        }
        std::fs::write(dir.path().join(format!("{repo}.db.tar.gz")), &archive).unwrap();
    }
    let client = aurcache_deps::AurClient::with_urls_and_paths(
        "http://unused.invalid/rpc/v5",
        dir.path().join("no-mirrorlist"),
        dir.path().to_path_buf(),
    );
    client.official.refresh().await.unwrap();
    TestClient {
        client,
        _repos: dir,
    }
}

#[tokio::test]
async fn a_dump_carries_configuration_and_not_history() {
    let db = aurcache_db::testing::memory_db().await;
    package(&db, "hello", true, None).await;

    let dump = build_dump(&db, "test", None).await.unwrap();
    let pkg = dump.packages.get("hello").expect("package missing");

    assert_eq!(pkg.platforms, ["aarch64", "x86_64"]);
    // The empty entry from `--noconfirm;;--nocolor` is dropped: it means the
    // same set, and a restore should not inherit the difference.
    assert_eq!(pkg.build_flags, ["--noconfirm", "--nocolor"]);
    assert!(pkg.directly_requested);

    // Nothing derived reaches the file. Asserted against the serialised form,
    // because that is what a restore reads.
    let json = serde_json::to_string(&dump.packages).unwrap();
    for derived in [
        "status",
        "out_of_date",
        "upstream_version",
        "split_packages",
        "latest_build",
    ] {
        assert!(
            !json.contains(derived),
            "the dump carries derived field {derived}: {json}"
        );
    }
}

/// A dependency stays a dependency. Restoring everything as
/// directly-requested would turn packages nobody asked for into packages the
/// user now has to manage.
#[tokio::test]
async fn a_dependency_is_not_promoted_to_a_request() {
    let db = aurcache_db::testing::memory_db().await;
    package(&db, "requested", true, None).await;
    package(&db, "pulled-in", false, None).await;

    let dump = build_dump(&db, "test", None).await.unwrap();
    assert!(dump.packages["requested"].directly_requested);
    assert!(!dump.packages["pulled-in"].directly_requested);
}

/// A patch travels as a file, and the flag mirrors it. The format's rule is
/// that a patch is present iff its file is, so the two cannot disagree.
#[tokio::test]
async fn a_patch_travels_as_a_file() {
    let db = aurcache_db::testing::memory_db().await;
    package(&db, "patched", true, Some("--- a\n+++ b\n")).await;
    package(&db, "plain", true, None).await;

    let dump = build_dump(&db, "test", None).await.unwrap();
    assert_eq!(
        dump.patches.get("patched").map(String::as_str),
        Some("--- a\n+++ b\n")
    );
    assert!(!dump.patches.contains_key("plain"));
    assert!(dump.packages["patched"].has_patch);
    assert!(!dump.packages["plain"].has_patch);

    let files = archive_files(&write_archive(&dump).unwrap());
    assert!(files.contains_key("patches/patched.patch"));
    assert!(!files.contains_key("patches/plain.patch"));
}

/// Settings are re-keyed from row ids onto pkgbase, which is the only name a
/// dump uses.
#[tokio::test]
async fn settings_are_keyed_by_pkgbase() {
    let db = aurcache_db::testing::memory_db().await;
    let pkg_id = package(&db, "hello", true, None).await;

    for (key, value, scope) in [
        ("version_check_interval", "600", None),
        ("build_flags", "--nocheck", Some(pkg_id)),
    ] {
        settings::ActiveModel {
            key: Set(key.to_string()),
            value: Set(Some(value.to_string())),
            pkg_id: Set(settings::scope(scope)),
            ..Default::default()
        }
        .insert(&db)
        .await
        .unwrap();
    }

    let dump = build_dump(&db, "test", None).await.unwrap();
    assert_eq!(
        dump.settings
            .global
            .get("version_check_interval")
            .map(String::as_str),
        Some("600")
    );
    assert_eq!(
        dump.settings.packages["hello"]
            .get("build_flags")
            .map(String::as_str),
        Some("--nocheck")
    );
}

/// Only approved workers. A pending enrolment is a decision nobody has made,
/// and restoring it would carry an unanswered question into the new instance.
#[tokio::test]
async fn only_approved_workers_are_carried() {
    let db = aurcache_db::testing::memory_db().await;
    for (name, status) in [
        (
            "trusted",
            aurcache_common::api::worker::ApprovalStatus::Approved,
        ),
        (
            "waiting",
            aurcache_common::api::worker::ApprovalStatus::Pending,
        ),
    ] {
        workers::ActiveModel {
            name: Set(name.to_string()),
            status: Set(status),
            cert_fingerprint: Set(format!("fp-{name}")),
            arches: Set("x86_64".parse().unwrap()),
            package_affinity: Set(Default::default()),
            priority: Set(0),
            concurrency: Set(1),
            settings_declaration: Set("[]".to_string()),
            ..Default::default()
        }
        .insert(&db)
        .await
        .unwrap();
    }

    let dump = build_dump(&db, "test", None).await.unwrap();
    let names: Vec<&str> = dump.workers.iter().map(|w| w.name.as_str()).collect();
    assert_eq!(names, ["trusted"]);
    // The fingerprint is the identity a re-enrolling worker is recognised by.
    assert_eq!(dump.workers[0].cert_fingerprint, "fp-trusted");
}

/// The archive is what an importer actually reads, so its shape is part of the
/// contract: the manifest first, and a schema version to refuse on.
#[tokio::test]
async fn the_archive_holds_the_documented_files() {
    let db = aurcache_db::testing::memory_db().await;
    package(&db, "hello", true, None).await;

    let dump = build_dump(&db, "test", None).await.unwrap();
    let files = archive_files(&write_archive(&dump).unwrap());

    for expected in [
        MANIFEST_FILE,
        PACKAGES_FILE,
        "settings.json",
        "workers.json",
    ] {
        assert!(files.contains_key(expected), "missing {expected}");
    }
    let manifest: serde_json::Value = serde_json::from_str(&files[MANIFEST_FILE]).unwrap();
    assert_eq!(manifest["schema_version"], DUMP_SCHEMA_VERSION);
    assert_eq!(manifest["includes_secrets"], false);
}

/// Two dumps of an unchanged instance differ only by their timestamp, so one
/// kept in git shows real changes rather than reordering.
#[tokio::test]
async fn the_output_is_ordered_and_stable() {
    let db = aurcache_db::testing::memory_db().await;
    for name in ["zzz", "aaa", "mmm"] {
        package(&db, name, true, None).await;
    }

    let first = build_dump(&db, "test", None).await.unwrap();
    let second = build_dump(&db, "test", None).await.unwrap();
    assert_eq!(
        serde_json::to_string(&first.packages).unwrap(),
        serde_json::to_string(&second.packages).unwrap()
    );
    let names: Vec<&str> = first.packages.keys().map(String::as_str).collect();
    assert_eq!(names, ["aaa", "mmm", "zzz"]);
}

/// A git-sourced package keeps its remote, ref and subfolder: that is the
/// whole of what identifies it, and none of it can be worked out again.
#[tokio::test]
async fn a_git_source_is_carried_whole() {
    let db = aurcache_db::testing::memory_db().await;
    packages::ActiveModel {
        name: Set("mine".to_string()),
        status: Set(BuildState::Active),
        out_of_date: Set(false),
        build_flags: Set(Default::default()),
        platforms: Set("x86_64".parse().unwrap()),
        source_data: Set(SourceData::Git {
            spec: GitSourceSpec {
                url: "https://example.com/mine.git".to_string(),
                r#ref: "v2".to_string(),
                subfolder: "pkg".to_string(),
            },
        }),
        directly_requested: Set(true),
        ..Default::default()
    }
    .insert(&db)
    .await
    .unwrap();

    let dump = build_dump(&db, "test", None).await.unwrap();
    match &dump.packages["mine"].source_data {
        SourceData::Git { spec } => {
            assert_eq!(spec.url, "https://example.com/mine.git");
            assert_eq!(spec.r#ref, "v2");
            assert_eq!(spec.subfolder, "pkg");
        }
        other => panic!("git source became {other:?}"),
    }
}

/// Read a `.tar.gz` into path -> contents.
fn archive_files(bytes: &[u8]) -> HashMap<String, String> {
    let mut archive = tar::Archive::new(GzDecoder::new(bytes));
    archive
        .entries()
        .unwrap()
        .map(|entry| {
            let mut entry = entry.unwrap();
            let path = entry.path().unwrap().to_string_lossy().into_owned();
            let mut content = String::new();
            entry.read_to_string(&mut content).unwrap();
            (path, content)
        })
        .collect()
}

/// Read a `.tar.gz` into path -> mode.
fn archive_modes(bytes: &[u8]) -> HashMap<String, u32> {
    let mut archive = tar::Archive::new(GzDecoder::new(bytes));
    archive
        .entries()
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            let path = entry.path().unwrap().to_string_lossy().into_owned();
            (path, entry.header().mode().unwrap())
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Restore
// ---------------------------------------------------------------------------

use aurcache_common::api::dump::{ExistingPackagePolicy, RestoreOptions, RestoreOutcome};
use aurcache_utils::restore::{load_dump, preview};

/// A dump of whatever is in `db`, as bytes.
async fn dump_bytes(db: &DatabaseConnection) -> Vec<u8> {
    write_archive(&build_dump(db, "test", None).await.unwrap()).unwrap()
}

/// A dump round-trips: what comes out describes what went in.
#[tokio::test]
async fn a_dump_reloads_as_what_it_described() {
    let source = aurcache_db::testing::memory_db().await;
    package(&source, "hello", true, Some("--- a\n")).await;
    package(&source, "dep", false, None).await;

    let loaded = load_dump(&dump_bytes(&source).await).unwrap();
    assert_eq!(loaded.packages.len(), 2);
    assert!(loaded.packages["hello"].directly_requested);
    assert!(!loaded.packages["dep"].directly_requested);
    assert_eq!(
        loaded.patches.get("hello").map(String::as_str),
        Some("--- a\n")
    );
}

/// A preview says what would happen and changes nothing -- which is the only
/// useful thing to know before importing over a live instance.
#[tokio::test]
async fn a_preview_reports_without_applying() {
    let source = aurcache_db::testing::memory_db().await;
    package(&source, "already-here", true, None).await;
    package(&source, "brand-new", true, None).await;
    let bytes = dump_bytes(&source).await;

    let target = aurcache_db::testing::memory_db().await;
    package(&target, "already-here", true, None).await;

    let loaded = load_dump(&bytes).unwrap();
    let entries = preview(&target, &loaded, &RestoreOptions::default())
        .await
        .unwrap();

    let by_name: HashMap<&str, &RestoreOutcome> = entries
        .iter()
        .map(|e| (e.pkgbase.as_str(), &e.outcome))
        .collect();
    assert!(matches!(by_name["already-here"], RestoreOutcome::Skipped));
    assert!(matches!(by_name["brand-new"], RestoreOutcome::Imported));

    // Nothing was written: the package the dump would have added is still absent.
    let count = Packages::find().all(&target).await.unwrap().len();
    assert_eq!(count, 1, "a preview wrote to the database");
}

/// Overwrite is opt-in, and it changes what the preview promises.
#[tokio::test]
async fn overwrite_is_reported_as_overwrite() {
    let source = aurcache_db::testing::memory_db().await;
    package(&source, "shared", true, None).await;
    let bytes = dump_bytes(&source).await;

    let target = aurcache_db::testing::memory_db().await;
    package(&target, "shared", true, None).await;

    let loaded = load_dump(&bytes).unwrap();
    let entries = preview(
        &target,
        &loaded,
        &RestoreOptions {
            dry_run: true,
            on_existing: ExistingPackagePolicy::Overwrite,
            ..RestoreOptions::default()
        },
    )
    .await
    .unwrap();
    assert!(matches!(entries[0].outcome, RestoreOutcome::Overwritten));
}

/// Skip leaves an existing package exactly as it was. The default has to be
/// the one that cannot destroy configuration nobody meant to replace.
#[tokio::test]
async fn skip_leaves_the_existing_configuration_alone() {
    let source = aurcache_db::testing::memory_db().await;
    package(&source, "shared", true, None).await;
    let bytes = dump_bytes(&source).await;

    let target = aurcache_db::testing::memory_db().await;
    let id = package(&target, "shared", true, None).await;
    // Something the dump does not carry, so a wrongly-applied overwrite shows.
    let mut existing: packages::ActiveModel = Packages::find_by_id(id)
        .one(&target)
        .await
        .unwrap()
        .unwrap()
        .into();
    existing.platforms = Set("aarch64".parse().unwrap());
    existing.update(&target).await.unwrap();

    let loaded = load_dump(&bytes).unwrap();
    let applied = aurcache_utils::restore::write_rows(
        &target,
        &test_repo(),
        &loaded,
        &RestoreOptions::default(),
    )
    .await
    .unwrap();

    let row = Packages::find_by_id(id)
        .one(&target)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        row.platforms.to_string(),
        "aarch64",
        "skip overwrote the target"
    );
    assert!(
        applied.touched.is_empty(),
        "a skipped package should not be re-resolved: it was already here"
    );
}

/// Overwrite replaces the configuration the dump owns.
#[tokio::test]
async fn overwrite_replaces_the_configuration() {
    let source = aurcache_db::testing::memory_db().await;
    package(&source, "shared", true, None).await;
    let bytes = dump_bytes(&source).await;

    let target = aurcache_db::testing::memory_db().await;
    let id = package(&target, "shared", true, None).await;
    let mut existing: packages::ActiveModel = Packages::find_by_id(id)
        .one(&target)
        .await
        .unwrap()
        .unwrap()
        .into();
    existing.platforms = Set("aarch64".parse().unwrap());
    existing.update(&target).await.unwrap();

    let loaded = load_dump(&bytes).unwrap();
    aurcache_utils::restore::write_rows(
        &target,
        &test_repo(),
        &loaded,
        &RestoreOptions {
            dry_run: false,
            on_existing: ExistingPackagePolicy::Overwrite,
            ..RestoreOptions::default()
        },
    )
    .await
    .unwrap();

    let row = Packages::find_by_id(id)
        .one(&target)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        row.platforms.to_string(),
        "aarch64;x86_64",
        "overwrite did not apply"
    );
}

/// Settings come back under the right package, having travelled by name.
#[tokio::test]
async fn settings_are_restored_against_the_right_package() {
    let source = aurcache_db::testing::memory_db().await;
    let pkg_id = package(&source, "hello", true, None).await;
    for (key, value, scope) in [
        ("version_check_interval", "600", None),
        ("max_artifact_size", "40G", Some(pkg_id)),
    ] {
        settings::ActiveModel {
            key: Set(key.to_string()),
            value: Set(Some(value.to_string())),
            pkg_id: Set(settings::scope(scope)),
            ..Default::default()
        }
        .insert(&source)
        .await
        .unwrap();
    }
    let bytes = dump_bytes(&source).await;

    let target = aurcache_db::testing::memory_db().await;
    let loaded = load_dump(&bytes).unwrap();
    aurcache_utils::restore::write_rows(&target, &test_repo(), &loaded, &RestoreOptions::default())
        .await
        .unwrap();

    let new_id = Packages::find().one(&target).await.unwrap().unwrap().id;
    let rows = settings::Entity::find().all(&target).await.unwrap();
    let global = rows
        .iter()
        .find(|r| r.key == "version_check_interval")
        .unwrap();
    assert_eq!(settings::package_of(global.pkg_id), None);
    let scoped = rows.iter().find(|r| r.key == "max_artifact_size").unwrap();
    assert_eq!(
        settings::package_of(scoped.pkg_id),
        Some(new_id),
        "a per-package setting landed on the wrong package"
    );
}

/// A setting this version does not know is not restored: nothing would read
/// its row.
#[tokio::test]
async fn an_unknown_setting_is_not_restored() {
    let source = aurcache_db::testing::memory_db().await;
    // Written after the migrations ran, which remove it.
    for key in ["cpu_limit", "version_check_interval"] {
        settings::ActiveModel {
            key: Set(key.to_string()),
            value: Set(Some("600".to_string())),
            pkg_id: Set(settings::scope(None)),
            ..Default::default()
        }
        .insert(&source)
        .await
        .unwrap();
    }
    let bytes = dump_bytes(&source).await;

    let target = aurcache_db::testing::memory_db().await;
    let loaded = load_dump(&bytes).unwrap();
    assert!(
        loaded.settings.global.contains_key("cpu_limit"),
        "the dump should carry it"
    );
    aurcache_utils::restore::write_rows(&target, &test_repo(), &loaded, &RestoreOptions::default())
        .await
        .unwrap();

    let keys: Vec<String> = settings::Entity::find()
        .all(&target)
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.key)
        .collect();
    assert!(
        keys.contains(&"version_check_interval".to_string()),
        "{keys:?}"
    );
    assert!(!keys.contains(&"cpu_limit".to_string()), "{keys:?}");
}

/// A package whose source cannot be read must be *reported*, not merely logged.
///
/// This is the failure that looks like success: the row is written by pass 1,
/// so a report that only counts pass 1 says everything was imported while the
/// package sits without the split package names and `provides` that let other
/// packages find it. Restoring a package whose git remote does not exist is the
/// cheapest way to reach that state.
#[tokio::test]
async fn a_package_whose_source_fails_is_reported_as_failed() {
    let source = aurcache_db::testing::memory_db().await;
    packages::ActiveModel {
        name: Set("broken".to_string()),
        status: Set(BuildState::Active),
        out_of_date: Set(false),
        build_flags: Set(Default::default()),
        platforms: Set("x86_64".parse().unwrap()),
        source_data: Set(SourceData::Git {
            spec: GitSourceSpec {
                // Unroutable by construction, so this needs no network to fail.
                url: "https://localhost:1/nope.git".to_string(),
                r#ref: "HEAD".to_string(),
                subfolder: String::new(),
            },
        }),
        directly_requested: Set(true),
        ..Default::default()
    }
    .insert(&source)
    .await
    .unwrap();
    let bytes = dump_bytes(&source).await;

    let target = aurcache_db::testing::memory_db().await;
    let loaded = load_dump(&bytes).unwrap();
    let store = std::sync::Arc::new(aurcache_utils::snapshot::SnapshotStore::with_checkout_root(
        tempfile::tempdir().unwrap().keep(),
    ));
    // Drained only after `apply` returns, so the bound must exceed the
    // dump's entries; production recorders drain live on the smaller
    // shared bound.
    let (progress_tx, mut progress_rx) = tokio::sync::mpsc::channel(1024);

    let TestClient { client, _repos } = client_with_empty_official_repos().await;
    aurcache_utils::restore::apply(
        &Services {
            db: target.clone(),
            store: store.clone(),
            client: Arc::new(client),
            repo: Arc::new(aurcache_utils::repository::Repository::new(
                tempfile::tempdir().unwrap().keep(),
            )),
            activity: ActivityLog::discarding(),
        },
        &tempfile::tempdir().unwrap().keep(),
        loaded,
        RestoreOptions::default(),
        Vec::new(),
        progress_tx,
    )
    .await;

    let mut entries = Vec::new();
    while let Ok(entry) = progress_rx.try_recv() {
        entries.push(entry);
    }

    // Imported first -- the row really was written -- and then failed, which is
    // the word that has to reach whoever is watching.
    assert!(
        matches!(
            entries.first().map(|e| &e.outcome),
            Some(RestoreOutcome::Imported)
        ),
        "expected the row to be written first: {entries:?}"
    );
    let failure = entries
        .iter()
        .find(|e| matches!(e.outcome, RestoreOutcome::Failed { .. }))
        .expect("a package whose source cannot be read was reported as a success");
    let RestoreOutcome::Failed { error } = &failure.outcome else {
        unreachable!()
    };
    // The message has to say what is wrong with the package now, not just that
    // something went wrong: it is on the instance but other packages cannot
    // find it.
    assert!(
        error.contains("will not find it"),
        "the failure does not say what it means: {error}"
    );
    assert!(
        error.contains("--on-existing overwrite"),
        "the failure does not say how to retry it: {error}"
    );

    // The row stays: it carries the configuration the dump asked for, and the
    // commonest reason to get here is a transient read.
    assert_eq!(Packages::find().all(&target).await.unwrap().len(), 1);
}

/// A restored package builds without waiting for the server to restart: the
/// restore queues what it brought in, as an add does.
#[tokio::test]
async fn a_restored_package_is_queued_to_build() {
    // A git source on disk, with a `.SRCINFO` that parses without the bridge.
    let remote = tempfile::tempdir().unwrap();
    let repo = git2::Repository::init(remote.path()).unwrap();
    std::fs::write(
        remote.path().join("PKGBUILD"),
        "pkgname=hello\npkgver=1.0\npkgrel=1\narch=('x86_64')\n",
    )
    .unwrap();
    std::fs::write(
        remote.path().join(".SRCINFO"),
        "pkgbase = hello\n\tpkgver = 1.0\n\tpkgrel = 1\n\tarch = x86_64\n\npkgname = hello\n",
    )
    .unwrap();
    let mut index = repo.index().unwrap();
    index.add_path(std::path::Path::new("PKGBUILD")).unwrap();
    index.add_path(std::path::Path::new(".SRCINFO")).unwrap();
    let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
    let sig = git2::Signature::now("Test", "test@example.com").unwrap();
    repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
        .unwrap();

    let source = aurcache_db::testing::memory_db().await;
    packages::ActiveModel {
        name: Set("hello".to_string()),
        status: Set(BuildState::Successful),
        out_of_date: Set(false),
        build_flags: Set(Default::default()),
        platforms: Set("x86_64".parse().unwrap()),
        source_data: Set(SourceData::Git {
            spec: GitSourceSpec {
                url: remote.path().to_string_lossy().to_string(),
                r#ref: "HEAD".to_string(),
                subfolder: String::new(),
            },
        }),
        directly_requested: Set(true),
        ..Default::default()
    }
    .insert(&source)
    .await
    .unwrap();
    let bytes = dump_bytes(&source).await;

    let target = aurcache_db::testing::memory_db().await;
    let (progress_tx, mut progress_rx) = tokio::sync::mpsc::channel(1024);
    let TestClient { client, _repos } = client_with_empty_official_repos().await;
    aurcache_utils::restore::apply(
        &Services {
            db: target.clone(),
            store: Arc::new(aurcache_utils::snapshot::SnapshotStore::with_checkout_root(
                tempfile::tempdir().unwrap().keep(),
            )),
            client: Arc::new(client),
            repo: Arc::new(test_repo()),
            activity: ActivityLog::discarding(),
        },
        &tempfile::tempdir().unwrap().keep(),
        load_dump(&bytes).unwrap(),
        RestoreOptions::default(),
        Vec::new(),
        progress_tx,
    )
    .await;
    while let Ok(entry) = progress_rx.try_recv() {
        assert!(
            !matches!(entry.outcome, RestoreOutcome::Failed { .. }),
            "{entry:?}"
        );
    }

    let builds = aurcache_db::prelude::Builds::find()
        .all(&target)
        .await
        .unwrap();
    assert_eq!(builds.len(), 1, "{builds:?}");
    assert_eq!(
        builds[0].status,
        aurcache_common::build_state::BuildState::Enqueued
    );
}

/// `--clear` replaces rather than adds: what was here and is not in the dump
/// is gone, and what the dump carries is present.
#[tokio::test]
async fn clear_replaces_what_was_here() {
    let source = aurcache_db::testing::memory_db().await;
    package(&source, "from-dump", true, None).await;
    let bytes = dump_bytes(&source).await;

    let target = aurcache_db::testing::memory_db().await;
    package(&target, "only-here", true, None).await;

    let loaded = load_dump(&bytes).unwrap();
    aurcache_utils::restore::write_rows(
        &target,
        &test_repo(),
        &loaded,
        &RestoreOptions {
            clear: true,
            ..RestoreOptions::default()
        },
    )
    .await
    .unwrap();

    let names: Vec<String> = Packages::find()
        .all(&target)
        .await
        .unwrap()
        .into_iter()
        .map(|p| p.name)
        .collect();
    assert_eq!(names, ["from-dump"], "clear did not replace the contents");
}

/// Approved workers come back, identified by the fingerprint they will present
/// on contact rather than by a name either instance chose.
#[tokio::test]
async fn workers_are_restored_by_fingerprint() {
    let source = aurcache_db::testing::memory_db().await;
    package(&source, "hello", true, None).await;
    workers::ActiveModel {
        name: Set("builder".to_string()),
        status: Set(aurcache_common::api::worker::ApprovalStatus::Approved),
        cert_fingerprint: Set("fp-1".to_string()),
        arches: Set("x86_64".parse().unwrap()),
        package_affinity: Set(Default::default()),
        priority: Set(7),
        concurrency: Set(3),
        settings_declaration: Set("[]".to_string()),
        ..Default::default()
    }
    .insert(&source)
    .await
    .unwrap();
    // Configuration someone chose for this machine travels with it.
    let source_id = workers::Entity::find()
        .one(&source)
        .await
        .unwrap()
        .unwrap()
        .id;
    aurcache_db::helpers::worker_store::save_worker_settings(
        &source,
        source_id,
        &std::collections::BTreeMap::from([("build_timeout".to_string(), Some("6h".to_string()))]),
    )
    .await
    .unwrap();
    let bytes = dump_bytes(&source).await;

    let target = aurcache_db::testing::memory_db().await;
    let loaded = load_dump(&bytes).unwrap();
    aurcache_utils::restore::write_rows(&target, &test_repo(), &loaded, &RestoreOptions::default())
        .await
        .unwrap();

    let restored = workers::Entity::find().all(&target).await.unwrap();
    assert_eq!(restored.len(), 1);
    assert_eq!(restored[0].cert_fingerprint, "fp-1");
    assert_eq!(restored[0].priority, 7);
    assert_eq!(restored[0].concurrency, 3);
    // No certificate travels: one signed by another instance's CA would mean
    // nothing here, so the worker re-enrols and is approved on its fingerprint.
    assert_eq!(restored[0].signed_cert, None);
    // Its values follow it, keyed to the row it has here.
    let values = aurcache_db::helpers::worker_store::worker_setting_values(&target, restored[0].id)
        .await
        .unwrap();
    assert_eq!(values.get("build_timeout").map(String::as_str), Some("6h"));
}

/// A worker's list columns are comma-joined, and the dump writes them as
/// real lists: one entry per architecture, in both directions. Splitting them
/// as the package columns are once dumped `["x86_64,aarch64"]`, and a dump
/// written by hand in the documented shape restored as one unknown arch.
#[tokio::test]
async fn a_workers_lists_travel_as_lists() {
    let source = aurcache_db::testing::memory_db().await;
    workers::ActiveModel {
        name: Set("builder".to_string()),
        status: Set(aurcache_common::api::worker::ApprovalStatus::Approved),
        cert_fingerprint: Set("fp-1".to_string()),
        arches: Set("x86_64,aarch64".parse().unwrap()),
        package_affinity: Set("big-one,other".parse().unwrap()),
        priority: Set(0),
        concurrency: Set(1),
        settings_declaration: Set("[]".to_string()),
        ..Default::default()
    }
    .insert(&source)
    .await
    .unwrap();

    let dump = build_dump(&source, "test", None).await.unwrap();
    assert_eq!(dump.workers[0].arches, ["x86_64", "aarch64"]);
    assert_eq!(dump.workers[0].package_affinity, ["big-one", "other"]);

    let target = aurcache_db::testing::memory_db().await;
    let loaded = load_dump(&dump_bytes(&source).await).unwrap();
    aurcache_utils::restore::write_rows(&target, &test_repo(), &loaded, &RestoreOptions::default())
        .await
        .unwrap();
    let restored = workers::Entity::find().one(&target).await.unwrap().unwrap();
    assert_eq!(restored.arches.to_string(), "x86_64,aarch64");
    assert_eq!(restored.package_affinity.to_string(), "big-one,other");
}

/// A worker already trusted here keeps the routing this instance gave it. The
/// dump says who to trust, not how this server should schedule.
#[tokio::test]
async fn an_already_trusted_worker_keeps_its_routing() {
    let source = aurcache_db::testing::memory_db().await;
    package(&source, "hello", true, None).await;
    workers::ActiveModel {
        name: Set("builder".to_string()),
        status: Set(aurcache_common::api::worker::ApprovalStatus::Approved),
        cert_fingerprint: Set("fp-1".to_string()),
        arches: Set("x86_64".parse().unwrap()),
        package_affinity: Set(Default::default()),
        priority: Set(7),
        concurrency: Set(3),
        settings_declaration: Set("[]".to_string()),
        ..Default::default()
    }
    .insert(&source)
    .await
    .unwrap();
    let bytes = dump_bytes(&source).await;

    let target = aurcache_db::testing::memory_db().await;
    workers::ActiveModel {
        name: Set("same-machine-other-name".to_string()),
        status: Set(aurcache_common::api::worker::ApprovalStatus::Approved),
        cert_fingerprint: Set("fp-1".to_string()),
        arches: Set("x86_64".parse().unwrap()),
        package_affinity: Set(Default::default()),
        priority: Set(1),
        concurrency: Set(9),
        settings_declaration: Set("[]".to_string()),
        ..Default::default()
    }
    .insert(&target)
    .await
    .unwrap();

    let loaded = load_dump(&bytes).unwrap();
    aurcache_utils::restore::write_rows(&target, &test_repo(), &loaded, &RestoreOptions::default())
        .await
        .unwrap();

    let all = workers::Entity::find().all(&target).await.unwrap();
    assert_eq!(all.len(), 1, "the same machine was trusted twice");
    assert_eq!(all[0].concurrency, 9, "the dump overrode local routing");
}

/// The default carries nothing dangerous. Asserted against the archive bytes
/// rather than the struct, because what leaves the process is what matters: a
/// dump kept beside ordinary backups must not contain a key that mints workers.
#[tokio::test]
async fn a_default_dump_carries_no_secrets() {
    let source = aurcache_db::testing::memory_db().await;
    package(&source, "hello", true, None).await;
    aurcache_db::api_tokens::ActiveModel {
        username: Set("alice".to_string()),
        token_hash: Set("secret-hash".to_string()),
        ..Default::default()
    }
    .insert(&source)
    .await
    .unwrap();

    let dump = build_dump(&source, "test", None).await.unwrap();
    assert!(dump.secrets.is_none());
    assert!(!dump.manifest.includes_secrets);

    let files = archive_files(&write_archive(&dump).unwrap());
    for forbidden in [
        "ca-cert.pem",
        "ca-key.pem",
        "tokens.json",
        "signing-key.asc",
        "ssh/id_ed25519",
        "ssh/known_hosts",
    ] {
        assert!(
            !files.contains_key(forbidden),
            "a public dump carries {forbidden}"
        );
    }
    // Not merely absent as a file -- absent as bytes, wherever they might have
    // been smuggled.
    let whole = files.values().cloned().collect::<String>();
    assert!(
        !whole.contains("secret-hash"),
        "a token hash leaked into a public dump"
    );
}

/// Asked for them, a dump carries the CA, the certificates and the hashes --
/// and says so in the manifest.
#[tokio::test]
async fn a_private_dump_carries_the_ca_and_says_so() {
    let source = aurcache_db::testing::memory_db().await;
    package(&source, "hello", true, None).await;
    aurcache_db::api_tokens::ActiveModel {
        username: Set("alice".to_string()),
        token_hash: Set("secret-hash".to_string()),
        ..Default::default()
    }
    .insert(&source)
    .await
    .unwrap();
    workers::ActiveModel {
        name: Set("builder".to_string()),
        status: Set(aurcache_common::api::worker::ApprovalStatus::Approved),
        cert_fingerprint: Set("fp-1".to_string()),
        signed_cert: Set(Some("-----BEGIN CERTIFICATE-----".to_string())),
        not_after: Set(Some(99)),
        arches: Set("x86_64".parse().unwrap()),
        package_affinity: Set(Default::default()),
        priority: Set(0),
        concurrency: Set(1),
        settings_declaration: Set("[]".to_string()),
        ..Default::default()
    }
    .insert(&source)
    .await
    .unwrap();

    let ca_dir = tempfile::tempdir().unwrap();
    let ca = aurcache_ca::Ca::load_or_create(ca_dir.path()).unwrap();
    // Locked, and carried as stored: locked, without its passphrase.
    let key_path = ca_dir.path().join(aurcache_signing::KEY_FILE);
    aurcache_signing::InstanceKey::load_or_create(&key_path, Some("correct horse")).unwrap();
    let key = std::fs::read(&key_path).unwrap();
    std::fs::write(ca_dir.path().join("id_ed25519"), "SSH KEY").unwrap();
    std::fs::write(ca_dir.path().join("known_hosts"), "HOSTS").unwrap();

    let dump = build_dump(&source, "test", Some(&secrets_in(ca_dir.path())))
        .await
        .unwrap();
    assert!(dump.manifest.includes_secrets);
    let secrets = dump.secrets.clone().unwrap();
    assert_eq!(secrets.ca_key_pem, ca.private_key_pem());
    assert_eq!(secrets.tokens.len(), 1);
    // The certificate travels only in this mode.
    assert!(dump.workers[0].signed_cert.is_some());
    // Nothing secret in what a log would show of it.
    let shown = format!("{secrets:?}");
    assert!(
        !shown.contains("PRIVATE KEY") && !shown.contains("secret-hash"),
        "{shown}"
    );

    let archive = write_archive(&dump).unwrap();
    let files = archive_files(&archive);
    assert_eq!(
        files.get("ca-key.pem").map(String::as_str),
        Some(ca.private_key_pem())
    );
    assert_eq!(files["signing-key.asc"].as_bytes(), key);
    assert!(
        !files
            .values()
            .any(|content| content.contains("correct horse"))
    );
    let modes = archive_modes(&archive);
    for secret in [
        "ca-cert.pem",
        "ca-key.pem",
        "tokens.json",
        "signing-key.asc",
        "ssh/id_ed25519",
        "ssh/known_hosts",
    ] {
        assert_eq!(modes[secret], 0o600, "{secret}");
    }
    assert_eq!(modes[MANIFEST_FILE], 0o644);

    // And it round-trips through the reader, which is what a restore does.
    let loaded = load_dump(&archive).unwrap().secrets.unwrap();
    assert_eq!(loaded.ca_key_pem, ca.private_key_pem());
    assert_eq!(loaded.signing_key, Some(key));
    assert_eq!(loaded.ssh_key.as_deref(), Some(&b"SSH KEY"[..]));
    assert_eq!(loaded.known_hosts.as_deref(), Some(&b"HOSTS"[..]));

    // A supplied file is the operator's, and stays out.
    let mut supplied = secrets_in(ca_dir.path());
    supplied.ssh = supplied_ssh(ca_dir.path());
    let dump = build_dump(&source, "test", Some(&supplied)).await.unwrap();
    let secrets = dump.secrets.unwrap();
    assert!(secrets.ssh_key.is_none() && secrets.known_hosts.is_none());
}

/// Ignoring secrets is the default on the way back in, too. Replacing a CA
/// invalidates every certificate the current workers hold, which is destructive
/// in the mode whose promise is that it only adds.
#[tokio::test]
async fn restoring_does_not_touch_the_ca_unless_asked() {
    let source = aurcache_db::testing::memory_db().await;
    package(&source, "hello", true, None).await;
    let src_ca = tempfile::tempdir().unwrap();
    aurcache_ca::Ca::load_or_create(src_ca.path()).unwrap();
    let bytes = write_archive(
        &build_dump(&source, "test", Some(&secrets_in(src_ca.path())))
            .await
            .unwrap(),
    )
    .unwrap();

    let target = aurcache_db::testing::memory_db().await;
    let target_ca = tempfile::tempdir().unwrap();
    std::fs::write(target_ca.path().join("ca-key.pem"), "MINE").unwrap();

    let store = store_with_hello();
    // Drained only after `apply` returns, so the bound must exceed the
    // dump's entries; production recorders drain live on the smaller
    // shared bound.
    let (progress_tx, _progress_rx) = tokio::sync::mpsc::channel(1024);

    let TestClient { client, _repos } = client_with_empty_official_repos().await;
    aurcache_utils::restore::apply(
        &Services {
            db: target.clone(),
            store: store.clone(),
            client: Arc::new(client),
            repo: Arc::new(aurcache_utils::repository::Repository::new(
                tempfile::tempdir().unwrap().keep(),
            )),
            activity: ActivityLog::discarding(),
        },
        target_ca.path(),
        load_dump(&bytes).unwrap(),
        RestoreOptions::default(),
        Vec::new(),
        progress_tx,
    )
    .await;

    assert_eq!(
        std::fs::read_to_string(target_ca.path().join("ca-key.pem")).unwrap(),
        "MINE",
        "a default restore replaced the CA"
    );
    // The token hashes are left alone for the same reason.
    assert!(
        aurcache_db::prelude::ApiTokens::find()
            .all(&target)
            .await
            .unwrap()
            .is_empty()
    );
}

/// Asked for them, they are taken -- and the key lands owner-readable only.
#[tokio::test]
async fn copying_secrets_replaces_the_ca_and_protects_the_key() {
    let source = aurcache_db::testing::memory_db().await;
    package(&source, "hello", true, None).await;
    aurcache_db::api_tokens::ActiveModel {
        username: Set("alice".to_string()),
        token_hash: Set("h".to_string()),
        ..Default::default()
    }
    .insert(&source)
    .await
    .unwrap();
    let src_ca = tempfile::tempdir().unwrap();
    aurcache_ca::Ca::load_or_create(src_ca.path()).unwrap();
    let source_key = std::fs::read_to_string(aurcache_ca::key_path(src_ca.path())).unwrap();
    let bytes = write_archive(
        &build_dump(&source, "test", Some(&secrets_in(src_ca.path())))
            .await
            .unwrap(),
    )
    .unwrap();

    let target = aurcache_db::testing::memory_db().await;
    let target_ca = tempfile::tempdir().unwrap();
    std::fs::write(target_ca.path().join("ca-key.pem"), "MINE").unwrap();

    let store = store_with_hello();
    // Drained only after `apply` returns, so the bound must exceed the
    // dump's entries; production recorders drain live on the smaller
    // shared bound.
    let (progress_tx, _progress_rx) = tokio::sync::mpsc::channel(1024);

    let TestClient { client, _repos } = client_with_empty_official_repos().await;
    aurcache_utils::restore::apply(
        &Services {
            db: target.clone(),
            store: store.clone(),
            client: Arc::new(client),
            repo: Arc::new(aurcache_utils::repository::Repository::new(
                tempfile::tempdir().unwrap().keep(),
            )),
            activity: ActivityLog::discarding(),
        },
        target_ca.path(),
        load_dump(&bytes).unwrap(),
        RestoreOptions {
            secrets: aurcache_common::api::dump::SecretsPolicy::Copy,
            ..RestoreOptions::default()
        },
        Vec::new(),
        progress_tx,
    )
    .await;

    let key_path = target_ca.path().join("ca-key.pem");
    assert_eq!(std::fs::read_to_string(&key_path).unwrap(), source_key);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&key_path).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "the CA private key is readable by others");
    }
    // Tokens users already hold keep working.
    let tokens = aurcache_db::prelude::ApiTokens::find()
        .all(&target)
        .await
        .unwrap();
    assert_eq!(tokens.len(), 1);
    assert_eq!(tokens[0].username, "alice");
}

/// `merge-patches` fills in a patch the instance lacks, and leaves the rest of
/// its configuration alone -- it is `skip` plus that one thing.
#[tokio::test]
async fn merge_patches_adopts_a_patch_the_instance_lacks() {
    let source = aurcache_db::testing::memory_db().await;
    package(&source, "shared", true, Some("--- from-dump\n")).await;
    let bytes = dump_bytes(&source).await;

    let target = aurcache_db::testing::memory_db().await;
    let id = package(&target, "shared", true, None).await;
    // Local configuration the dump does not match, to show it survives.
    let mut existing: packages::ActiveModel = Packages::find_by_id(id)
        .one(&target)
        .await
        .unwrap()
        .unwrap()
        .into();
    existing.platforms = Set("aarch64".parse().unwrap());
    existing.update(&target).await.unwrap();

    let loaded = load_dump(&bytes).unwrap();
    let applied = aurcache_utils::restore::write_rows(
        &target,
        &test_repo(),
        &loaded,
        &RestoreOptions {
            on_existing: ExistingPackagePolicy::MergePatches,
            ..RestoreOptions::default()
        },
    )
    .await
    .unwrap();

    let row = Packages::find_by_id(id)
        .one(&target)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.patch.as_deref(), Some("--- from-dump\n"));
    assert_eq!(
        row.platforms.to_string(),
        "aarch64",
        "merge-patches overwrote configuration"
    );
    assert!(matches!(
        applied.entries[0].outcome,
        RestoreOutcome::PatchAdopted
    ));
    // A patch changes what the source resolves to, so it has to be read again.
    assert_eq!(applied.touched, ["shared"]);
}

/// The instance's own patch is never replaced by the dump's absence of one.
#[tokio::test]
async fn merge_patches_keeps_the_instances_own_patch() {
    let source = aurcache_db::testing::memory_db().await;
    package(&source, "shared", true, None).await;
    let bytes = dump_bytes(&source).await;

    let target = aurcache_db::testing::memory_db().await;
    let id = package(&target, "shared", true, Some("--- mine\n")).await;

    let loaded = load_dump(&bytes).unwrap();
    let applied = aurcache_utils::restore::write_rows(
        &target,
        &test_repo(),
        &loaded,
        &RestoreOptions {
            on_existing: ExistingPackagePolicy::MergePatches,
            ..RestoreOptions::default()
        },
    )
    .await
    .unwrap();

    let row = Packages::find_by_id(id)
        .one(&target)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.patch.as_deref(), Some("--- mine\n"));
    assert!(matches!(
        applied.entries[0].outcome,
        RestoreOutcome::Skipped
    ));
    // Nothing changed, so nothing needs re-reading.
    assert!(applied.touched.is_empty());
}

/// Two patches for the same package are not merged and not silently picked
/// between: the import is refused, and nothing is written.
#[tokio::test]
async fn two_patches_for_one_package_refuse_the_import() {
    let source = aurcache_db::testing::memory_db().await;
    package(&source, "shared", true, Some("--- from-dump\n")).await;
    package(&source, "also-new", true, None).await;
    let bytes = dump_bytes(&source).await;

    let target = aurcache_db::testing::memory_db().await;
    package(&target, "shared", true, Some("--- mine\n")).await;

    let loaded = load_dump(&bytes).unwrap();
    let error = aurcache_utils::restore::write_rows(
        &target,
        &test_repo(),
        &loaded,
        &RestoreOptions {
            on_existing: ExistingPackagePolicy::MergePatches,
            ..RestoreOptions::default()
        },
    )
    .await
    .unwrap_err()
    .to_string();

    assert!(error.contains("shared"), "the conflict is unnamed: {error}");
    assert!(
        error.contains("--on-existing overwrite"),
        "the refusal offers no way forward: {error}"
    );
    // Refused before anything was written: the other package in the dump did
    // not land either.
    let names: Vec<String> = Packages::find()
        .all(&target)
        .await
        .unwrap()
        .into_iter()
        .map(|p| p.name)
        .collect();
    assert_eq!(names, ["shared"], "a refused import wrote something anyway");
}

/// A package is refused when another package already builds its name, and the
/// import writes nothing.
#[tokio::test]
async fn a_name_another_package_builds_refuses_the_import() {
    let source = aurcache_db::testing::memory_db().await;
    package(&source, "foo", true, None).await;
    let loaded = load_dump(&dump_bytes(&source).await).unwrap();

    let target = aurcache_db::testing::memory_db().await;
    let bar = package(&target, "bar", true, None).await;
    let mut row: packages::ActiveModel = Packages::find_by_id(bar)
        .one(&target)
        .await
        .unwrap()
        .unwrap()
        .into();
    row.split_packages = Set(Some(r#"["foo"]"#.parse().unwrap()));
    row.update(&target).await.unwrap();

    let error = aurcache_utils::restore::write_rows(
        &target,
        &test_repo(),
        &loaded,
        &RestoreOptions::default(),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("'foo' is already built by package 'bar'"),
        "{error}"
    );
    assert_eq!(Packages::find().all(&target).await.unwrap().len(), 1);
}

/// A dry run reports the conflict rather than discovering it on the real run,
/// which is the whole reason to offer one.
#[tokio::test]
async fn a_preview_names_the_conflicting_packages() {
    let source = aurcache_db::testing::memory_db().await;
    package(&source, "clash", true, Some("--- from-dump\n")).await;
    package(&source, "fine", true, None).await;
    let bytes = dump_bytes(&source).await;

    let target = aurcache_db::testing::memory_db().await;
    package(&target, "clash", true, Some("--- mine\n")).await;

    let loaded = load_dump(&bytes).unwrap();
    let entries = preview(
        &target,
        &loaded,
        &RestoreOptions {
            dry_run: true,
            on_existing: ExistingPackagePolicy::MergePatches,
            ..RestoreOptions::default()
        },
    )
    .await
    .unwrap();

    let by_name: HashMap<&str, &RestoreOutcome> = entries
        .iter()
        .map(|e| (e.pkgbase.as_str(), &e.outcome))
        .collect();
    assert!(matches!(by_name["clash"], RestoreOutcome::Failed { .. }));
    assert!(matches!(by_name["fine"], RestoreOutcome::Imported));
}

/// A one-commit git repository holding a package's PKGBUILD and `.SRCINFO`,
/// as a source a restore can read without the network.
fn git_package(dir: &std::path::Path, name: &str, depends: &[&str]) -> GitSourceSpec {
    let repo = git2::Repository::init(dir).unwrap();
    let depends_srcinfo: String = depends
        .iter()
        .map(|dep| format!("    depends = {dep}\n"))
        .collect();
    std::fs::write(
        dir.join(".SRCINFO"),
        format!(
            "pkgbase = {name}\n    pkgver = 1.0\n    pkgrel = 1\n    arch = x86_64\n\
             {depends_srcinfo}\npkgname = {name}\n"
        ),
    )
    .unwrap();
    std::fs::write(
        dir.join("PKGBUILD"),
        format!("pkgname={name}\npkgver=1.0\npkgrel=1\narch=('x86_64')\n"),
    )
    .unwrap();
    let mut index = repo.index().unwrap();
    index.add_path(std::path::Path::new(".SRCINFO")).unwrap();
    index.add_path(std::path::Path::new("PKGBUILD")).unwrap();
    index.write().unwrap();
    let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
    let sig = git2::Signature::now("Test", "test@example.com").unwrap();
    repo.commit(Some("HEAD"), &sig, &sig, "initial", &tree, &[])
        .unwrap();
    GitSourceSpec {
        url: dir.to_string_lossy().into_owned(),
        r#ref: "HEAD".to_string(),
        subfolder: String::new(),
    }
}

/// A dependency-only package the dump carries survives the restore, and is
/// what its dependent ends up depending on.
///
/// Edges are not exported, so until pass 3 reaches its dependent such a
/// package has nothing pointing at it. Resyncing one package at a time with an
/// orphan sweep after each deleted it the moment an earlier package -- by name
/// order -- was resynced, and the dependent then went looking for it in the
/// AUR.
#[tokio::test]
async fn a_dependency_only_package_survives_the_restore() {
    let source = aurcache_db::testing::memory_db().await;
    let sources = tempfile::tempdir().unwrap();
    for (name, requested, depends) in [
        ("aaa", true, &[][..]),
        ("mylib", false, &[][..]),
        ("zzz", true, &["mylib"][..]),
    ] {
        let dir = sources.path().join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let spec = git_package(&dir, name, depends);
        packages::ActiveModel {
            name: Set(name.to_string()),
            status: Set(BuildState::Active),
            out_of_date: Set(false),
            build_flags: Set(Default::default()),
            platforms: Set("x86_64".parse().unwrap()),
            source_data: Set(SourceData::Git { spec }),
            directly_requested: Set(requested),
            ..Default::default()
        }
        .insert(&source)
        .await
        .unwrap();
    }
    let bytes = dump_bytes(&source).await;

    let target = aurcache_db::testing::memory_db().await;
    let TestClient { client, _repos } = client_with_empty_official_repos().await;
    let (progress_tx, mut progress_rx) = tokio::sync::mpsc::channel(1024);
    aurcache_utils::restore::apply(
        &Services {
            db: target.clone(),
            store: Arc::new(aurcache_utils::snapshot::SnapshotStore::with_checkout_root(
                tempfile::tempdir().unwrap().keep(),
            )),
            client: Arc::new(client),
            repo: Arc::new(test_repo()),
            activity: ActivityLog::discarding(),
        },
        &tempfile::tempdir().unwrap().keep(),
        load_dump(&bytes).unwrap(),
        RestoreOptions::default(),
        Vec::new(),
        progress_tx,
    )
    .await;
    let mut failures = Vec::new();
    while let Ok(entry) = progress_rx.try_recv() {
        if matches!(entry.outcome, RestoreOutcome::Failed { .. }) {
            failures.push(entry);
        }
    }
    assert!(failures.is_empty(), "{failures:?}");

    let rows = Packages::find().all(&target).await.unwrap();
    let id_of = |name: &str| {
        rows.iter()
            .find(|row| row.name == name)
            .unwrap_or_else(|| panic!("{name} was not restored, or was removed again"))
            .id
    };
    let edges = aurcache_db::prelude::Dependencies::find()
        .all(&target)
        .await
        .unwrap();
    assert!(
        edges
            .iter()
            .any(|edge| edge.dependent_id == id_of("zzz") && edge.dependee_id == id_of("mylib")),
        "zzz does not depend on the restored mylib: {edges:?}"
    );
}

/// A clear removes every package first, so a patch here conflicts with
/// nothing: the dry run and the import both take the dump's.
#[tokio::test]
async fn a_clear_has_no_patch_to_conflict_with() {
    let source = aurcache_db::testing::memory_db().await;
    package(&source, "shared", true, Some("--- from-dump\n")).await;
    let bytes = dump_bytes(&source).await;

    let target = aurcache_db::testing::memory_db().await;
    package(&target, "shared", true, Some("--- mine\n")).await;
    let loaded = load_dump(&bytes).unwrap();
    let options = RestoreOptions {
        on_existing: ExistingPackagePolicy::MergePatches,
        clear: true,
        ..RestoreOptions::default()
    };

    let entries = preview(&target, &loaded, &options).await.unwrap();
    assert!(
        matches!(&entries[0].outcome, RestoreOutcome::Imported),
        "{entries:?}"
    );
    aurcache_utils::restore::write_rows(&target, &test_repo(), &loaded, &options)
        .await
        .unwrap();
    let row = Packages::find().one(&target).await.unwrap().unwrap();
    assert_eq!(row.patch.as_deref(), Some("--- from-dump\n"));
}
// ---------------------------------------------------------------------------
// The signing key
// ---------------------------------------------------------------------------

use aurcache_common::api::dump::{FileOutcome, SecretFile, SecretFileReport, SecretsPolicy};
use aurcache_signing::{InstanceKey, KEY_FILE, KeyConfig, KeyFile};
use aurcache_utils::restore::{FilePlan, plan_files};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const PASSPHRASE: &str = "correct horse";

/// The file of a new signing key, locked with `passphrase` when there is one.
fn new_key(passphrase: Option<&str>) -> Vec<u8> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(KEY_FILE);
    InstanceKey::load_or_create(&path, passphrase).unwrap();
    std::fs::read(path).unwrap()
}

/// A dump with secrets of one package, `hello`, carrying `key` as its
/// signing key.
async fn dump_with_key(key: Option<&[u8]>) -> Vec<u8> {
    let ca = tempfile::tempdir().unwrap();
    if let Some(key) = key {
        std::fs::write(ca.path().join(KEY_FILE), key).unwrap();
    }
    dump_from(ca.path()).await
}

/// A dump with secrets of one package, `hello`, from a server whose CA and
/// SSH files are in `dir`.
async fn dump_from(dir: &Path) -> Vec<u8> {
    let source = aurcache_db::testing::memory_db().await;
    package(&source, "hello", true, None).await;
    aurcache_ca::Ca::load_or_create(dir).unwrap();
    let dump = build_dump(&source, "test", Some(&secrets_in(dir))).await;
    write_archive(&dump.unwrap()).unwrap()
}

/// A server whose key file, `KEY_FILE` in `ca_dir`, holds `mine` and is kept
/// as `kind` says, unlocked with `passphrase`.
fn server_key(
    ca_dir: &Path,
    kind: fn(PathBuf) -> KeyFile,
    mine: &[u8],
    passphrase: Option<&str>,
) -> KeyConfig {
    std::fs::write(ca_dir.join(KEY_FILE), mine).unwrap();
    let passphrase_file = passphrase.map(|passphrase| {
        let path = ca_dir.join("passphrase");
        std::fs::write(&path, passphrase).unwrap();
        path
    });
    KeyConfig {
        key: kind(ca_dir.join(KEY_FILE)),
        passphrase_file,
    }
}

fn take_secrets() -> RestoreOptions {
    RestoreOptions {
        secrets: SecretsPolicy::Copy,
        ..RestoreOptions::default()
    }
}

/// Every file in `dir`, by name.
fn contents(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            let name = entry.file_name().to_string_lossy().into_owned();
            (name, std::fs::read(entry.path()).unwrap())
        })
        .collect()
}

/// The files in `dir` named `name`, or after it, sorted: the file, and any
/// copy kept of one.
fn files_named(dir: &Path, name: &str) -> Vec<String> {
    let names = contents(dir).into_keys();
    names.filter(|found| found.starts_with(name)).collect()
}

/// The name under which a restore keeps `bytes`, the old content of `name`.
fn kept_as(name: &str, bytes: &[u8]) -> String {
    let hash = aurcache_common::digest::sha256_hex(bytes);
    format!("{name}.replaced-{}", &hash[..16])
}

/// What a whole restore did, as the route runs it.
struct Restored {
    reports: Vec<SecretFileReport>,
    /// The errors of the entries that failed.
    failures: Vec<String>,
    /// Whether `hello` was imported.
    imported: bool,
    /// The kinds of the activity log's entries.
    recorded: Vec<String>,
}

impl Restored {
    /// What the restore did with the dump's `file`.
    fn outcome(&self, file: SecretFile) -> Option<FileOutcome> {
        let report = self.reports.iter().find(|r| r.file == file);
        report.map(|r| r.outcome)
    }

    fn recorded(&self, kind: &str) -> bool {
        self.recorded.iter().any(|k| k == kind)
    }
}

/// Plan the files and apply `bytes` to a new instance whose CA directory is
/// `ca_dir`, as the restore route does.
async fn restore_with(
    bytes: &[u8],
    ca_dir: &Path,
    config: &KeyConfig,
    ssh: &SshFiles,
    options: RestoreOptions,
) -> Restored {
    let dump = load_dump(bytes).unwrap();
    let before = contents(ca_dir);
    let plan = plan_files(&dump, &options, config, ssh).unwrap();
    assert_eq!(contents(ca_dir), before, "planning wrote");
    let reports = plan.iter().map(FilePlan::report).collect();

    let target = aurcache_db::testing::memory_db().await;
    let writer = aurcache_activitylog::activity_utils::spawn(target.clone());
    let (progress_tx, mut progress_rx) = tokio::sync::mpsc::channel(1024);
    let TestClient { client, _repos } = client_with_empty_official_repos().await;
    let services = Services {
        db: target.clone(),
        store: store_with_hello(),
        client: Arc::new(client),
        repo: Arc::new(test_repo()),
        activity: writer.log,
    };
    aurcache_utils::restore::apply(&services, ca_dir, dump, options, plan, progress_tx).await;
    // With the last handle gone, the writer drains and ends.
    drop(services);
    writer.task.await.unwrap();

    let mut restored = Restored {
        reports,
        failures: Vec::new(),
        imported: false,
        recorded: aurcache_db::prelude::Logs::find()
            .all(&target)
            .await
            .unwrap()
            .into_iter()
            .map(|row| row.kind)
            .collect(),
    };
    while let Ok(entry) = progress_rx.try_recv() {
        match entry.outcome {
            RestoreOutcome::Failed { error } => restored.failures.push(error),
            RestoreOutcome::Imported => restored.imported |= entry.pkgbase == "hello",
            _ => {}
        }
    }
    restored
}

/// A key that this server could not use is refused before anything is
/// written.
#[tokio::test]
async fn a_key_this_server_cannot_use_is_refused() {
    let source = tempfile::tempdir().unwrap();
    std::fs::write(source.path().join(KEY_FILE), new_key(Some(PASSPHRASE))).unwrap();
    std::fs::write(source.path().join(SSH_KEY), "not a key").unwrap();
    let dump = load_dump(&dump_from(source.path()).await).unwrap();
    let refused = |passphrase, said: &str| {
        let ca = tempfile::tempdir().unwrap();
        let config = server_key(ca.path(), KeyFile::Own, b"MINE", Some(passphrase));
        let before = contents(ca.path());
        let ssh = SshFiles::own(ca.path());
        let error = plan_files(&dump, &take_secrets(), &config, &ssh).unwrap_err();
        let error = format!("{error:#}");
        assert!(error.contains(said), "{error}");
        assert_eq!(contents(ca.path()), before, "planning wrote");
    };

    // The signing key is checked first, and does not unlock here.
    refused("wrong horse", "cannot sign here: the key does not sign");
    refused(PASSPHRASE, "SSH key (ssh/id_ed25519) cannot be used");
}

/// The server's key file is written only when the dump's key is new to a
/// server keeping its own, and then the old file is kept beside it; the rest
/// of the restore goes ahead whatever happens to the key.
#[tokio::test]
async fn a_restore_replaces_only_a_key_of_the_servers_own() {
    use FileOutcome::{KeptSupplied, Replaced, Unchanged};
    struct Case {
        name: &'static str,
        dump_key: Option<Vec<u8>>,
        kind: fn(PathBuf) -> KeyFile,
        mine: Vec<u8>,
        options: RestoreOptions,
        outcome: Option<FileOutcome>,
    }
    let key = new_key(Some(PASSPHRASE));
    // Another of the dump's keys, over this server's own.
    let case = |name| Case {
        name,
        dump_key: Some(key.clone()),
        kind: KeyFile::Own,
        mine: b"MINE".to_vec(),
        options: take_secrets(),
        outcome: Some(Replaced),
    };
    let cases = [
        case("another key"),
        Case {
            mine: key.clone(),
            outcome: Some(Unchanged),
            ..case("the same key")
        },
        // Not even parsed.
        Case {
            dump_key: Some(b"not a key".to_vec()),
            kind: KeyFile::Supplied,
            outcome: Some(KeptSupplied),
            ..case("a supplied key")
        },
        Case {
            dump_key: Some(b"not a key".to_vec()),
            options: RestoreOptions::default(),
            outcome: None,
            ..case("secrets not taken")
        },
        Case {
            dump_key: None,
            options: RestoreOptions {
                clear: true,
                ..RestoreOptions::default()
            },
            outcome: None,
            ..case("a dump from before dumps carried the key, with --clear")
        },
    ];
    for case in cases {
        let name = case.name;
        let ca = tempfile::tempdir().unwrap();
        let config = server_key(ca.path(), case.kind, &case.mine, Some(PASSPHRASE));
        let bytes = dump_with_key(case.dump_key.as_deref()).await;

        let ssh = SshFiles::own(ca.path());
        let restored = restore_with(&bytes, ca.path(), &config, &ssh, case.options).await;
        assert_eq!(
            restored.outcome(SecretFile::SigningKey),
            case.outcome,
            "{name}"
        );
        assert!(restored.imported, "{name}");
        assert_eq!(restored.failures, Vec::<String>::new(), "{name}");
        let replaced = case.outcome == Some(Replaced);
        let recorded = restored.recorded("restore.signing_key_replaced");
        assert_eq!(recorded, replaced, "{name}");
        let path = ca.path().join(KEY_FILE);
        if replaced {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::read(&path).unwrap(), key);
            // The server would load it at its next start.
            InstanceKey::load(&path, Some(PASSPHRASE)).unwrap();
            let kept = ca.path().join(kept_as(KEY_FILE, &case.mine));
            assert_eq!(std::fs::read(&kept).unwrap(), case.mine);
            let mode = std::fs::metadata(&kept).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
            assert_eq!(
                files_named(ca.path(), KEY_FILE),
                [KEY_FILE.to_string(), kept_as(KEY_FILE, &case.mine)]
            );
        } else {
            assert_eq!(std::fs::read(&path).unwrap(), case.mine, "{name}");
            assert_eq!(files_named(ca.path(), KEY_FILE), [KEY_FILE], "{name}");
        }
    }
}

/// A key that cannot be written once the rows commit is reported as a failed
/// entry, and the server keeps its own.
#[tokio::test]
async fn a_key_that_cannot_be_written_is_reported() {
    use std::os::unix::fs::PermissionsExt;
    let source = tempfile::tempdir().unwrap();
    std::fs::write(source.path().join(KEY_FILE), new_key(None)).unwrap();
    std::fs::write(source.path().join(SSH_KEY), new_ssh_key()).unwrap();
    let bytes = dump_from(source.path()).await;
    let ca = tempfile::tempdir().unwrap();
    let config = server_key(ca.path(), KeyFile::Own, b"MINE", None);
    std::fs::write(ca.path().join(SSH_KEY), "MINE").unwrap();
    let permissions = |mode| std::fs::Permissions::from_mode(mode);
    std::fs::set_permissions(ca.path(), permissions(0o500)).unwrap();
    if std::fs::write(ca.path().join("probe"), "").is_ok() {
        std::fs::set_permissions(ca.path(), permissions(0o700)).unwrap();
        eprintln!("skipped: a read-only directory is writable here (root?)");
        return;
    }

    let ssh = SshFiles::own(ca.path());
    let restored = restore_with(&bytes, ca.path(), &config, &ssh, take_secrets()).await;
    std::fs::set_permissions(ca.path(), permissions(0o700)).unwrap();
    assert_eq!(
        restored.outcome(SecretFile::SigningKey),
        Some(FileOutcome::Replaced)
    );
    assert!(restored.imported);
    for file in ["Signing key", "SSH key"] {
        let said = format!("{file}: not written, though the packages were imported");
        assert!(
            restored.failures.iter().any(|error| error.contains(&said)),
            "{:?}",
            restored.failures
        );
    }
    assert!(!restored.recorded("restore.signing_key_replaced"));
    assert!(!restored.recorded("restore.ssh_key_replaced"));
    assert_eq!(std::fs::read(ca.path().join(KEY_FILE)).unwrap(), b"MINE");
    assert_eq!(std::fs::read(ca.path().join(SSH_KEY)).unwrap(), b"MINE");
}

// ---------------------------------------------------------------------------
// The SSH files
// ---------------------------------------------------------------------------

const SSH_KEY: &str = "id_ed25519";
const KNOWN_HOSTS: &str = "known_hosts";

fn new_ssh_key() -> Vec<u8> {
    let key =
        ssh_key::PrivateKey::random(&mut ssh_key::rand_core::OsRng, ssh_key::Algorithm::Ed25519)
            .unwrap();
    key.to_openssh(ssh_key::LineEnding::LF)
        .unwrap()
        .as_bytes()
        .to_vec()
}

/// The server's own SSH files are replaced by the dump's, each old one kept
/// beside it; a supplied file is never written, and one the dump does not
/// carry stays as it is.
#[tokio::test]
async fn a_restore_replaces_only_ssh_files_of_the_servers_own() {
    use FileOutcome::{KeptSupplied, Replaced, Unchanged};
    struct Case {
        name: &'static str,
        /// The server that the dump was taken from had the files.
        dumped: bool,
        /// This server already holds the dump's files.
        same: bool,
        supplied: bool,
        outcome: Option<FileOutcome>,
    }
    let case = |name| Case {
        name,
        dumped: true,
        same: false,
        supplied: false,
        outcome: Some(Replaced),
    };
    let cases = [
        case("other files"),
        Case {
            same: true,
            outcome: Some(Unchanged),
            ..case("the same files")
        },
        Case {
            supplied: true,
            outcome: Some(KeptSupplied),
            ..case("supplied files")
        },
        Case {
            dumped: false,
            outcome: None,
            ..case("a dump from before dumps carried them")
        },
    ];
    let key = new_ssh_key();
    let hosts = b"github.com ssh-ed25519 AAAA\n".to_vec();
    for case in cases {
        let name = case.name;
        let source = tempfile::tempdir().unwrap();
        if case.dumped {
            std::fs::write(source.path().join(SSH_KEY), &key).unwrap();
            std::fs::write(source.path().join(KNOWN_HOSTS), &hosts).unwrap();
        }
        let bytes = dump_from(source.path()).await;
        let ca = tempfile::tempdir().unwrap();
        let (mine_key, mine_hosts) = if case.same {
            (key.clone(), hosts.clone())
        } else {
            (b"MINE".to_vec(), b"MINE HOSTS\n".to_vec())
        };
        std::fs::write(ca.path().join(SSH_KEY), &mine_key).unwrap();
        std::fs::write(ca.path().join(KNOWN_HOSTS), &mine_hosts).unwrap();
        let config = server_key(ca.path(), KeyFile::Own, b"", None);
        let ssh = if case.supplied {
            supplied_ssh(ca.path())
        } else {
            SshFiles::own(ca.path())
        };

        let restored = restore_with(&bytes, ca.path(), &config, &ssh, take_secrets()).await;
        for file in [SecretFile::SshKey, SecretFile::KnownHosts] {
            assert_eq!(restored.outcome(file), case.outcome, "{name}: {file:?}");
        }
        assert!(restored.imported, "{name}");
        assert_eq!(restored.failures, Vec::<String>::new(), "{name}");
        let replaced = case.outcome == Some(Replaced);
        assert_eq!(
            restored.recorded("restore.ssh_key_replaced"),
            replaced,
            "{name}"
        );
        for (file, theirs, mine) in [
            (SSH_KEY, &key, &mine_key),
            (KNOWN_HOSTS, &hosts, &mine_hosts),
        ] {
            let now = std::fs::read(ca.path().join(file)).unwrap();
            if replaced {
                assert_eq!(&now, theirs, "{name}: {file}");
                let kept = kept_as(file, mine);
                assert_eq!(std::fs::read(ca.path().join(&kept)).unwrap(), *mine);
                assert_eq!(files_named(ca.path(), file), [file.to_string(), kept]);
            } else {
                assert_eq!(&now, mine, "{name}: {file}");
                assert_eq!(files_named(ca.path(), file), [file], "{name}");
            }
        }
        if replaced {
            // The server loads it at its next start, and its public half
            // follows.
            let access = aurcache_utils::ssh::SshAccess::new(
                ssh.key.clone(),
                ssh.known_hosts.clone(),
                ActivityLog::discarding(),
            );
            let aurcache_common::api::ssh::SshKey::Ready { public_key, .. } = access.key() else {
                panic!("the restored key does not load");
            };
            let public = std::fs::read_to_string(ca.path().join("id_ed25519.pub")).unwrap();
            assert_eq!(public.trim(), public_key);
        }
    }
}
