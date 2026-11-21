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
use flate2::read::GzDecoder;
use sea_orm::{ActiveModelTrait, DatabaseConnection, EntityTrait, Set};
use std::collections::HashMap;
use std::io::Read;
use std::sync::Arc;

/// A repository of its own for a restore that publishes nothing new.
fn test_repo() -> aurcache_utils::repository::Repository {
    aurcache_utils::repository::Repository::new(tempfile::tempdir().unwrap().keep())
}

/// What a restore into the CA directory `ca` is planned for on a server that
/// does not sign, as an unset environment configures it.
fn unsigned_secrets(ca: &std::path::Path) -> aurcache_utils::restore::SecretsConfig {
    aurcache_utils::restore::SecretsConfig {
        ca_dir: ca.to_path_buf(),
        location: aurcache_signing::KeyLocation::Own(ca.join(aurcache_signing::KEY_FILE)),
        passphrase_file: None,
        signs: false,
    }
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
/// Each file's mode in the archive, by path.
fn archive_modes(bytes: &[u8]) -> HashMap<String, u32> {
    let mut archive = tar::Archive::new(GzDecoder::new(bytes));
    archive
        .entries()
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            (
                entry.path().unwrap().to_string_lossy().into_owned(),
                entry.header().mode().unwrap(),
            )
        })
        .collect()
}

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

// ---------------------------------------------------------------------------
// Restore
// ---------------------------------------------------------------------------

use aurcache_common::api::dump::{ExistingPackagePolicy, RestoreOptions, RestoreOutcome};
use aurcache_utils::repository::Repository;
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
    let ca = tempfile::tempdir().unwrap();
    let entries = preview(
        &target,
        &Repository::new(ca.path()),
        &loaded,
        &RestoreOptions::default(),
        unsigned_secrets(ca.path()),
    )
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
        &Repository::new(tempfile::tempdir().unwrap().keep()),
        &loaded,
        &RestoreOptions {
            dry_run: true,
            on_existing: ExistingPackagePolicy::Overwrite,
            ..RestoreOptions::default()
        },
        unsigned_secrets(tempfile::tempdir().unwrap().path()),
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
        unsigned_secrets(&tempfile::tempdir().unwrap().keep()),
        loaded,
        RestoreOptions::default(),
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
        unsigned_secrets(&tempfile::tempdir().unwrap().keep()),
        load_dump(&bytes).unwrap(),
        RestoreOptions::default(),
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
    for forbidden in ["ca-cert.pem", "ca-key.pem", "tokens.json"] {
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
    // Nor anything about signing: no key, and nothing a restore would check.
    assert!(!files.contains_key("signing-key.asc"));
    assert!(!whole.contains("PGP"));
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
    // The repository's signing key, beside the CA and locked: it travels as
    // stored, still locked, and its passphrase does not travel at all.
    let key_file = ca_dir.path().join(aurcache_signing::KEY_FILE);
    aurcache_signing::InstanceKey::load_or_create(&key_file, Some("correct horse")).unwrap();
    let stored_key = std::fs::read_to_string(&key_file).unwrap();

    let dump = build_dump(
        &source,
        "test",
        Some(SecretSources {
            ca_dir: ca_dir.path(),
        }),
    )
    .await
    .unwrap();
    assert!(dump.manifest.includes_secrets);
    let secrets = dump.secrets.clone().unwrap();
    assert_eq!(secrets.ca_key_pem, ca.private_key_pem());
    assert_eq!(secrets.tokens.len(), 1);
    assert_eq!(secrets.signing_key.as_deref(), Some(stored_key.as_str()));
    // The certificate travels only in this mode.
    assert!(dump.workers[0].signed_cert.is_some());

    let files = archive_files(&write_archive(&dump).unwrap());
    assert_eq!(
        files.get("ca-key.pem").map(String::as_str),
        Some(ca.private_key_pem())
    );
    assert_eq!(
        files.get("signing-key.asc").map(String::as_str),
        Some(stored_key.as_str())
    );
    let modes = archive_modes(&write_archive(&dump).unwrap());
    for secret in ["ca-key.pem", "tokens.json", "signing-key.asc"] {
        assert_eq!(modes[secret], 0o600, "{secret} is readable by everyone");
    }
    assert_eq!(modes["manifest.json"], 0o644);
    assert!(
        !files
            .values()
            .any(|content| content.contains("correct horse")),
        "the signing key's passphrase travelled with it"
    );

    // And it round-trips through the reader, which is what a restore does.
    let loaded = load_dump(&write_archive(&dump).unwrap()).unwrap();
    let loaded = loaded.secrets.unwrap();
    assert_eq!(loaded.ca_key_pem, ca.private_key_pem());
    assert_eq!(loaded.signing_key.as_deref(), Some(stored_key.as_str()));
}

/// A server without a signing key dumps its secrets without one.
#[tokio::test]
async fn a_private_dump_of_a_server_without_a_signing_key_carries_none() {
    let source = aurcache_db::testing::memory_db().await;
    let ca_dir = tempfile::tempdir().unwrap();
    aurcache_ca::Ca::load_or_create(ca_dir.path()).unwrap();

    let dump = build_dump(
        &source,
        "test",
        Some(SecretSources {
            ca_dir: ca_dir.path(),
        }),
    )
    .await
    .unwrap();
    assert_eq!(dump.secrets.as_ref().unwrap().signing_key, None);
    let files = archive_files(&write_archive(&dump).unwrap());
    assert!(!files.contains_key("signing-key.asc"));
    assert!(load_dump(&write_archive(&dump).unwrap()).is_ok());
}

/// A signing key without the CA it travels with is a dump assembled wrongly.
#[test]
fn a_signing_key_without_secrets_is_refused() {
    let mut archive = Vec::new();
    {
        let encoder = flate2::write::GzEncoder::new(&mut archive, flate2::Compression::default());
        let mut builder = tar::Builder::new(encoder);
        let manifest = format!(
            "{{\"schema_version\":{DUMP_SCHEMA_VERSION},\"aurcache_version\":\"t\",\
             \"created_at\":0,\"includes_secrets\":false}}"
        );
        for (path, content) in [
            (MANIFEST_FILE, manifest.as_str()),
            (PACKAGES_FILE, "{}"),
            ("settings.json", "{\"global\":{},\"packages\":{}}"),
            ("workers.json", "[]"),
            ("signing-key.asc", "-----BEGIN PGP PRIVATE KEY BLOCK-----"),
        ] {
            let mut header = tar::Header::new_gnu();
            header.set_path(path).unwrap();
            header.set_size(content.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append(&header, content.as_bytes()).unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap();
    }
    let error = load_dump(&archive).err().unwrap();
    assert!(error.to_string().contains("no CA"), "{error:#}");
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
        &build_dump(
            &source,
            "test",
            Some(SecretSources {
                ca_dir: src_ca.path(),
            }),
        )
        .await
        .unwrap(),
    )
    .unwrap();

    let target = aurcache_db::testing::memory_db().await;
    let target_ca = tempfile::tempdir().unwrap();
    std::fs::write(target_ca.path().join("ca-key.pem"), "MINE").unwrap();

    let store = std::sync::Arc::new(aurcache_utils::snapshot::SnapshotStore::with_checkout_root(
        tempfile::tempdir().unwrap().keep(),
    ));
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
        unsigned_secrets(target_ca.path()),
        load_dump(&bytes).unwrap(),
        RestoreOptions::default(),
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
        &build_dump(
            &source,
            "test",
            Some(SecretSources {
                ca_dir: src_ca.path(),
            }),
        )
        .await
        .unwrap(),
    )
    .unwrap();

    let target = aurcache_db::testing::memory_db().await;
    let target_ca = tempfile::tempdir().unwrap();
    std::fs::write(target_ca.path().join("ca-key.pem"), "MINE").unwrap();

    let store = std::sync::Arc::new(aurcache_utils::snapshot::SnapshotStore::with_checkout_root(
        tempfile::tempdir().unwrap().keep(),
    ));
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
        unsigned_secrets(target_ca.path()),
        load_dump(&bytes).unwrap(),
        RestoreOptions {
            secrets: aurcache_common::api::dump::SecretsPolicy::Copy,
            ..RestoreOptions::default()
        },
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
        &Repository::new(tempfile::tempdir().unwrap().keep()),
        &loaded,
        &RestoreOptions {
            dry_run: true,
            on_existing: ExistingPackagePolicy::MergePatches,
            ..RestoreOptions::default()
        },
        unsigned_secrets(tempfile::tempdir().unwrap().path()),
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
        unsigned_secrets(&tempfile::tempdir().unwrap().keep()),
        load_dump(&bytes).unwrap(),
        RestoreOptions::default(),
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

mod signing_key {
    use super::*;
    use aurcache_common::api::dump::{RestoreEntry, Secret, SecretChange, SecretsPolicy};
    use aurcache_signing::{InstanceKey, KEY_FILE, KeyLocation};
    use aurcache_utils::restore::{
        CaOutcome, KeyOutcome, SecretsConfig, SignsWith, apply, apply_secrets, load_dump,
        plan_secrets, preview,
    };
    use aurcache_utils::signing::{Backfill, backfill};
    use pacman_mirrors::platforms::Platform;
    use std::path::Path;

    fn copy_secrets() -> RestoreOptions {
        RestoreOptions {
            secrets: SecretsPolicy::Copy,
            ..RestoreOptions::default()
        }
    }

    fn clear() -> RestoreOptions {
        RestoreOptions {
            clear: true,
            ..RestoreOptions::default()
        }
    }

    /// The server's own key in `ca`, no passphrase: what an unset environment
    /// configures, on a server that does not sign.
    fn own(ca: &Path) -> SecretsConfig {
        super::unsigned_secrets(ca)
    }

    /// As [`own`], with `passphrase` configured: in a file of its own, as
    /// `AURCACHE_SIGNING_KEY_PASSPHRASE_FILE` would name it.
    fn with_passphrase(ca: &Path, passphrase: &str) -> SecretsConfig {
        let file = tempfile::tempdir().unwrap().keep().join("passphrase");
        std::fs::write(&file, passphrase).unwrap();
        SecretsConfig {
            passphrase_file: Some(file),
            ..own(ca)
        }
    }

    /// The names in `dir`, sorted: what a refused restore must leave as it
    /// found it, temporary files included.
    fn listing(dir: &Path) -> Vec<std::ffi::OsString> {
        let mut names: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        names.sort();
        names
    }

    /// A dump with secrets, and the CA directory of the instance it was taken
    /// from, which goes when this does.
    struct SourceDump {
        bytes: Vec<u8>,
        ca: tempfile::TempDir,
    }

    /// The signing key the instance a dump is taken from has.
    #[derive(Clone, Copy)]
    enum SourceKey {
        Absent,
        Unlocked,
        Locked(&'static str),
    }

    /// A dump with secrets of an instance with no packages and `key`.
    async fn dump_with_key(key: SourceKey) -> SourceDump {
        let source = aurcache_db::testing::memory_db().await;
        let ca = tempfile::tempdir().unwrap();
        aurcache_ca::Ca::load_or_create(ca.path()).unwrap();
        let path = ca.path().join(KEY_FILE);
        match key {
            SourceKey::Absent => {}
            SourceKey::Unlocked => {
                InstanceKey::load_or_create(&path, None).unwrap();
            }
            SourceKey::Locked(passphrase) => {
                InstanceKey::load_or_create(&path, Some(passphrase)).unwrap();
            }
        }
        let sources = SecretSources { ca_dir: ca.path() };
        let bytes =
            write_archive(&build_dump(&source, "test", Some(sources)).await.unwrap()).unwrap();
        SourceDump { bytes, ca }
    }

    /// A dump without secrets of an instance with no packages.
    async fn dump_without_secrets() -> Vec<u8> {
        let source = aurcache_db::testing::memory_db().await;
        write_archive(&build_dump(&source, "test", None).await.unwrap()).unwrap()
    }

    /// Restore `bytes` with `options`, and what the restore reported.
    async fn restore(
        db: &DatabaseConnection,
        ca: &Path,
        repo: Arc<Repository>,
        bytes: &[u8],
        options: RestoreOptions,
    ) -> Vec<RestoreEntry> {
        let config = SecretsConfig {
            signs: repo.key().is_some(),
            ..own(ca)
        };
        restore_with(db, config, repo, bytes, options).await
    }

    /// Restore `bytes` with `options`, its secrets as `config` has them, and
    /// what the restore reported.
    async fn restore_with(
        db: &DatabaseConnection,
        config: SecretsConfig,
        repo: Arc<Repository>,
        bytes: &[u8],
        options: RestoreOptions,
    ) -> Vec<RestoreEntry> {
        let store = Arc::new(aurcache_utils::snapshot::SnapshotStore::with_checkout_root(
            tempfile::tempdir().unwrap().keep(),
        ));
        let (progress_tx, mut progress_rx) = tokio::sync::mpsc::channel(1024);
        let TestClient { client, _repos } = client_with_empty_official_repos().await;
        apply(
            &Services {
                db: db.clone(),
                store,
                client: Arc::new(client),
                repo,
                activity: ActivityLog::discarding(),
            },
            config,
            load_dump(bytes).unwrap(),
            options,
            progress_tx,
        )
        .await;
        let mut entries = Vec::new();
        while let Ok(entry) = progress_rx.try_recv() {
            entries.push(entry);
        }
        entries
    }

    /// What a restore's report says became of one secret.
    #[derive(Debug)]
    struct SecretLine<'a> {
        change: SecretChange,
        detail: &'a str,
    }

    /// The line `entries` give `secret`, which every restore that got past
    /// planning reports.
    fn secret_line(entries: &[RestoreEntry], secret: Secret) -> SecretLine<'_> {
        let lines: Vec<SecretLine<'_>> = entries
            .iter()
            .filter_map(|entry| match &entry.outcome {
                RestoreOutcome::Secret {
                    secret: which,
                    change,
                    detail,
                } if *which == secret => Some(SecretLine {
                    change: *change,
                    detail,
                }),
                _ => None,
            })
            .collect();
        assert_eq!(lines.len(), 1, "{entries:?}");
        lines.into_iter().next().unwrap()
    }

    /// That the file at `path` is readable by its owner only.
    fn assert_private(path: &Path) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "{}", path.display());
        }
    }

    fn failures(entries: &[RestoreEntry]) -> Vec<&str> {
        entries
            .iter()
            .filter_map(|entry| match &entry.outcome {
                RestoreOutcome::Failed { error } => Some(error.as_str()),
                _ => None,
            })
            .collect()
    }

    /// Publish a one-file package `pkgname` 1.0-1 through `repo`.
    async fn publish(db: &DatabaseConnection, repo: &Repository, pkgname: &str) {
        let pkginfo = format!("pkgname = {pkgname}\npkgver = 1.0-1\n");
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
        let dir = repo.staging_dir(1);
        std::fs::create_dir_all(&dir).unwrap();
        let staged = dir.join(format!("{pkgname}-1.0-1-x86_64.pkg.tar.zst"));
        std::fs::write(
            &staged,
            zstd::stream::encode_all(tar_bytes.as_slice(), 0).unwrap(),
        )
        .unwrap();
        let entry = repo.describe(staged.clone()).await.unwrap();
        let mut update = repo.begin(db).await.unwrap();
        update.add(Platform::X86_64, staged, entry);
        update
            .commit(db, |_| Box::pin(async { Ok(()) }))
            .await
            .unwrap();
    }

    /// Whether `key` signed `path`, beside it, as it is now.
    fn signed_beside(key: &InstanceKey, path: &Path) -> bool {
        let mut signature = path.as_os_str().to_owned();
        signature.push(".sig");
        std::fs::read(signature)
            .is_ok_and(|signature| key.verifies(&signature, &std::fs::read(path).unwrap()))
    }

    /// A server signing with a key of its own, generated in `ca`, over a
    /// repository in `root` with one package published.
    async fn signing_server(db: &DatabaseConnection, ca: &Path, root: &Path) -> Arc<Repository> {
        aurcache_ca::Ca::load_or_create(ca).unwrap();
        let key = InstanceKey::load_or_create(&ca.join(KEY_FILE), None).unwrap();
        let repo = Arc::new(Repository::new(root).with_key(key));
        repo.init().unwrap();
        publish(db, &repo, "hello").await;
        repo
    }

    /// Everything listed in `root`'s x86_64 repository, and its databases,
    /// carry `key`'s signature, beside each file and in each entry.
    fn all_signed_by(key: &InstanceKey, root: &Path) {
        let dir = root.join("x86_64");
        let listed = pacman_repo_utils::listed_packages(&dir.join("repo.db.tar.gz")).unwrap();
        assert!(!listed.is_empty());
        for entry in listed {
            let package = dir.join(&entry.filename);
            assert!(key.verifies(
                entry.signature.as_deref().unwrap(),
                &std::fs::read(&package).unwrap()
            ));
            assert!(signed_beside(key, &package));
        }
        assert!(signed_beside(key, &dir.join("repo.db.tar.gz")));
        assert!(signed_beside(key, &dir.join("repo.files.tar.gz")));
    }

    /// The point of carrying the key: a fresh instance restored from a dump
    /// with secrets signs with the key its clients already trust. The key a
    /// fresh start generated is replaced on disk and in use, and the backfill
    /// signs again what it signed, the databases included.
    #[tokio::test]
    async fn a_restored_key_replaces_a_generated_one_and_signs_everything() {
        let SourceDump {
            bytes,
            ca: source_ca,
        } = dump_with_key(SourceKey::Unlocked).await;
        let original = std::fs::read_to_string(source_ca.path().join(KEY_FILE)).unwrap();
        let original_fingerprint = aurcache_signing::inspect(original.as_bytes())
            .unwrap()
            .fingerprint;

        let target = aurcache_db::testing::memory_db().await;
        let target_ca = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let repo = signing_server(&target, target_ca.path(), root.path()).await;
        let fresh_fingerprint = repo.key().unwrap().fingerprint().to_string();
        let fresh = std::fs::read(target_ca.path().join(KEY_FILE)).unwrap();
        let kept = target_ca
            .path()
            .join(format!("{KEY_FILE}.replaced-{fresh_fingerprint}"));

        // A dry run says what becomes of both secrets, and where the key it
        // replaces would go -- and writes nothing.
        let files = listing(target_ca.path());
        let previewed = preview(
            &target,
            &repo,
            &load_dump(&bytes).unwrap(),
            &copy_secrets(),
            SecretsConfig {
                signs: true,
                ..own(target_ca.path())
            },
        )
        .await
        .unwrap();
        assert!(failures(&previewed).is_empty(), "{previewed:?}");
        assert_eq!(
            secret_line(&previewed, Secret::Ca).change,
            SecretChange::Replaced
        );
        let line = secret_line(&previewed, Secret::SigningKey);
        assert_eq!(line.change, SecretChange::Replaced);
        for said in [
            original_fingerprint.as_str(),
            &format!("in place of {fresh_fingerprint}"),
            &format!("kept as {}", kept.display()),
        ] {
            assert!(line.detail.contains(said), "{said}: {}", line.detail);
        }
        assert_eq!(listing(target_ca.path()), files);

        let entries = restore(
            &target,
            target_ca.path(),
            Arc::clone(&repo),
            &bytes,
            copy_secrets(),
        )
        .await;
        assert!(failures(&entries).is_empty(), "{entries:?}");
        assert_eq!(
            std::fs::read_to_string(target_ca.path().join(KEY_FILE)).unwrap(),
            original,
            "the dump's key, as stored, replaces the generated one"
        );
        // The generated key is not lost: kept beside it, owner-only, and the
        // restore says where.
        assert_eq!(std::fs::read(&kept).unwrap(), fresh);
        assert_private(&kept);
        assert_eq!(
            secret_line(&entries, Secret::Ca).change,
            SecretChange::Replaced
        );
        let line = secret_line(&entries, Secret::SigningKey);
        assert_eq!(line.change, SecretChange::Replaced);
        assert!(
            line.detail.contains(&format!("kept as {}", kept.display())),
            "{}",
            line.detail
        );
        let key = repo.key().unwrap();
        assert_eq!(key.fingerprint(), original_fingerprint);
        assert_ne!(key.fingerprint(), fresh_fingerprint);
        assert!(repo.signing_info().unwrap().backfilling);

        assert_eq!(
            backfill(&target, &repo).await.unwrap(),
            Backfill::Complete {
                signed: 1,
                skipped: Vec::new(),
            }
        );
        all_signed_by(&key, root.path());
        repo.mark_signed(&key);
        assert!(!repo.signing_info().unwrap().backfilling);
    }

    /// `--clear` with a dump carrying no key leaves the server as a new one
    /// starts: a server that signs with its own key gets a newly generated
    /// one, and the backfill signs everything with it.
    #[tokio::test]
    async fn clearing_without_a_key_starts_over_with_a_new_one() {
        let target = aurcache_db::testing::memory_db().await;
        let target_ca = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let repo = signing_server(&target, target_ca.path(), root.path()).await;
        let before = repo.key().unwrap().fingerprint().to_string();

        let entries = restore(
            &target,
            target_ca.path(),
            Arc::clone(&repo),
            &dump_without_secrets().await,
            clear(),
        )
        .await;
        assert!(failures(&entries).is_empty(), "{entries:?}");
        let key = repo.key().unwrap();
        assert_ne!(key.fingerprint(), before);
        let kept = target_ca
            .path()
            .join(format!("{KEY_FILE}.replaced-{before}"));
        assert_private(&kept);
        assert_eq!(secret_line(&entries, Secret::Ca).change, SecretChange::Kept);
        let line = secret_line(&entries, Secret::SigningKey);
        assert_eq!(line.change, SecretChange::Generated);
        assert!(
            line.detail.contains(key.fingerprint())
                && line.detail.contains(&format!("kept as {}", kept.display())),
            "{}",
            line.detail
        );
        let on_disk = std::fs::read(target_ca.path().join(KEY_FILE)).unwrap();
        assert_eq!(
            aurcache_signing::inspect(&on_disk).unwrap().fingerprint,
            key.fingerprint()
        );
        assert_eq!(
            backfill(&target, &repo).await.unwrap(),
            Backfill::Complete {
                signed: 1,
                skipped: Vec::new(),
            }
        );
        all_signed_by(&key, root.path());
    }

    /// `--clear` on a server that does not sign removes its key file, as a
    /// new instance has none; a dump with a key puts that one in place.
    #[tokio::test]
    async fn clearing_removes_the_key_or_takes_the_dumps() {
        let ca = tempfile::tempdir().unwrap();
        aurcache_ca::Ca::load_or_create(ca.path()).unwrap();
        let removed = InstanceKey::load_or_create(&ca.path().join(KEY_FILE), None)
            .unwrap()
            .fingerprint()
            .to_string();
        let removed_file = std::fs::read(ca.path().join(KEY_FILE)).unwrap();
        let repo = Repository::new(tempfile::tempdir().unwrap().keep());

        let without = load_dump(&dump_without_secrets().await).unwrap();
        let plan = plan_secrets(&without, &clear(), own(ca.path()))
            .await
            .unwrap();
        let applied = apply_secrets(plan, &repo).await;
        let KeyOutcome::Reset {
            generated: None,
            supplied: None,
            file,
        } = applied.signing_key
        else {
            panic!("{applied:?}");
        };
        assert!(!ca.path().join(KEY_FILE).exists());
        // Removed, but not lost: kept under its fingerprint, owner-only.
        let kept = ca.path().join(format!("{KEY_FILE}.replaced-{removed}"));
        assert_eq!(file.previous.fingerprint(), Some(removed.as_str()));
        assert_eq!(file.previous.kept(), Some(kept.as_path()));
        assert_eq!(std::fs::read(&kept).unwrap(), removed_file);
        assert_private(&kept);

        let SourceDump {
            bytes,
            ca: _source_ca,
        } = dump_with_key(SourceKey::Unlocked).await;
        let with = load_dump(&bytes).unwrap();
        let plan = plan_secrets(&with, &clear(), own(ca.path())).await.unwrap();
        let applied = apply_secrets(plan, &repo).await;
        assert!(matches!(
            applied.signing_key,
            KeyOutcome::Installed {
                signs_with: SignsWith::Nothing,
                ..
            }
        ));
        assert_eq!(
            std::fs::read_to_string(ca.path().join(KEY_FILE)).unwrap(),
            with.secrets.unwrap().signing_key.unwrap()
        );
    }

    /// Without `--clear` and without taking the dump's secrets -- the
    /// default -- the key is left exactly as it is, even one the dump carries
    /// locked with a passphrase this server does not have.
    #[tokio::test]
    async fn a_default_restore_leaves_the_key_alone() {
        let SourceDump {
            bytes,
            ca: _source_ca,
        } = dump_with_key(SourceKey::Locked("correct horse")).await;
        let ca = tempfile::tempdir().unwrap();
        InstanceKey::load_or_create(&ca.path().join(KEY_FILE), None).unwrap();
        let before = std::fs::read(ca.path().join(KEY_FILE)).unwrap();
        let dump = load_dump(&bytes).unwrap();
        let plan = plan_secrets(&dump, &RestoreOptions::default(), own(ca.path()))
            .await
            .unwrap();
        let applied = apply_secrets(plan, &Repository::new(ca.path())).await;
        assert_eq!(applied.signing_key, KeyOutcome::Kept);
        assert_eq!(applied.ca, CaOutcome::Kept);
        assert_eq!(std::fs::read(ca.path().join(KEY_FILE)).unwrap(), before);
    }

    /// A key locked with a passphrase this server does not have would stop
    /// it at its next start, so the restore is refused whole, saying what is
    /// missing -- and a dry run says so first.
    #[tokio::test]
    async fn a_locked_key_without_its_passphrase_is_refused_before_anything_changes() {
        let SourceDump {
            bytes,
            ca: _source_ca,
        } = dump_with_key(SourceKey::Locked("correct horse")).await;

        let target = aurcache_db::testing::memory_db().await;
        let target_ca = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let repo = signing_server(&target, target_ca.path(), root.path()).await;
        let ca_key = std::fs::read(target_ca.path().join("ca-key.pem")).unwrap();
        let key_file = std::fs::read(target_ca.path().join(KEY_FILE)).unwrap();
        let fingerprint = repo.key().unwrap().fingerprint().to_string();
        let files = listing(target_ca.path());

        let refusal = |entries: &[RestoreEntry]| {
            failures(entries).iter().any(|error| {
                error.contains("nothing") && error.contains("AURCACHE_SIGNING_KEY_PASSPHRASE_FILE")
            })
        };
        let previewed = preview(
            &target,
            &repo,
            &load_dump(&bytes).unwrap(),
            &copy_secrets(),
            SecretsConfig {
                signs: repo.key().is_some(),
                ..own(target_ca.path())
            },
        )
        .await
        .unwrap();
        assert!(refusal(&previewed), "{previewed:?}");
        assert_eq!(listing(target_ca.path()), files);

        let entries = restore(
            &target,
            target_ca.path(),
            Arc::clone(&repo),
            &bytes,
            copy_secrets(),
        )
        .await;
        assert!(refusal(&entries), "{entries:?}");
        assert_eq!(
            std::fs::read(target_ca.path().join("ca-key.pem")).unwrap(),
            ca_key,
            "the CA was replaced all the same"
        );
        assert_eq!(
            std::fs::read(target_ca.path().join(KEY_FILE)).unwrap(),
            key_file
        );
        assert_eq!(repo.key().unwrap().fingerprint(), fingerprint);
        assert_eq!(
            listing(target_ca.path()),
            files,
            "something was left staged"
        );
    }

    /// A dump whose CA is not a pair is refused before anything is written.
    #[tokio::test]
    async fn a_ca_that_is_not_a_pair_is_refused_before_anything_changes() {
        let SourceDump {
            bytes,
            ca: _source_ca,
        } = dump_with_key(SourceKey::Absent).await;
        let mut dump = load_dump(&bytes).unwrap();
        let other = tempfile::tempdir().unwrap();
        let other_ca = aurcache_ca::Ca::load_or_create(other.path()).unwrap();
        dump.secrets.as_mut().unwrap().ca_key_pem = other_ca.private_key_pem().to_string();

        let ca = tempfile::tempdir().unwrap();
        let error = plan_secrets(&dump, &copy_secrets(), own(ca.path()))
            .await
            .err()
            .unwrap();
        assert!(format!("{error:#}").contains("CA"), "{error:#}");
    }

    /// A CA directory the server cannot write to refuses the restore before
    /// anything is written, rather than halfway through.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_read_only_ca_directory_is_refused_before_anything_changes() {
        use std::os::unix::fs::PermissionsExt;
        let SourceDump {
            bytes,
            ca: _source_ca,
        } = dump_with_key(SourceKey::Unlocked).await;
        let dump = load_dump(&bytes).unwrap();
        let ca = tempfile::tempdir().unwrap();
        std::fs::set_permissions(ca.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
        if std::fs::File::create(ca.path().join("probe")).is_ok() {
            eprintln!("skipped: permissions do not apply here (running as root?)");
            return;
        }
        let refused = plan_secrets(&dump, &copy_secrets(), own(ca.path())).await;
        std::fs::set_permissions(ca.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(refused.is_err());
    }

    /// Restoring the same dump twice replaces nothing the second time: the
    /// CA is the one in place already, and the key file holds the very bytes
    /// written again, so neither is reported replaced, and no copy of the
    /// key is kept. A key locked anew with this server's passphrase is other
    /// bytes, so what the file held is kept beside it all the same: the same
    /// key can hold more than the copy written over it.
    #[tokio::test]
    async fn the_same_dump_restored_twice_replaces_nothing_the_second_time() {
        let SourceDump {
            bytes,
            ca: _source_ca,
        } = dump_with_key(SourceKey::Unlocked).await;
        let target = aurcache_db::testing::memory_db().await;
        let ca = tempfile::tempdir().unwrap();
        aurcache_ca::Ca::load_or_create(ca.path()).unwrap();
        let repo = Arc::new(Repository::new(tempfile::tempdir().unwrap().keep()));
        let first = restore_with(
            &target,
            own(ca.path()),
            Arc::clone(&repo),
            &bytes,
            copy_secrets(),
        )
        .await;
        assert!(failures(&first).is_empty(), "{first:?}");
        assert_eq!(
            secret_line(&first, Secret::Ca).change,
            SecretChange::Replaced
        );
        let key_file = std::fs::read(ca.path().join(KEY_FILE)).unwrap();
        let files = listing(ca.path());

        let second = restore_with(
            &target,
            own(ca.path()),
            Arc::clone(&repo),
            &bytes,
            copy_secrets(),
        )
        .await;
        assert!(failures(&second).is_empty(), "{second:?}");
        assert_eq!(secret_line(&second, Secret::Ca).change, SecretChange::Kept);
        assert_eq!(
            secret_line(&second, Secret::SigningKey).change,
            SecretChange::Kept
        );
        assert_eq!(std::fs::read(ca.path().join(KEY_FILE)).unwrap(), key_file);
        assert_eq!(listing(ca.path()), files, "nothing more is kept");

        let locked = restore_with(
            &target,
            with_passphrase(ca.path(), "correct horse"),
            Arc::clone(&repo),
            &bytes,
            copy_secrets(),
        )
        .await;
        assert!(failures(&locked).is_empty(), "{locked:?}");
        assert_eq!(
            secret_line(&locked, Secret::SigningKey).change,
            SecretChange::Replaced
        );
        let fingerprint = aurcache_signing::inspect(&key_file).unwrap().fingerprint;
        let kept = ca.path().join(format!("{KEY_FILE}.replaced-{fingerprint}"));
        assert_eq!(std::fs::read(&kept).unwrap(), key_file);
        assert_private(&kept);
    }

    /// A key the operator supplies still signs: the restored one goes to the
    /// server's own location, never to the supplied path, and the restore
    /// says the two differ.
    #[tokio::test]
    async fn a_supplied_key_still_signs_after_a_restore() {
        let SourceDump {
            bytes,
            ca: _source_ca,
        } = dump_with_key(SourceKey::Unlocked).await;
        let dump = load_dump(&bytes).unwrap();

        let target_ca = tempfile::tempdir().unwrap();
        let supplied_dir = tempfile::tempdir().unwrap();
        let supplied_path = supplied_dir.path().join("operator.asc");
        let supplied = InstanceKey::load_or_create(&supplied_path, None).unwrap();
        let supplied_fingerprint = supplied.fingerprint().to_string();
        let supplied_file = std::fs::read(&supplied_path).unwrap();
        let repo = Repository::new(tempfile::tempdir().unwrap().keep()).with_key(supplied);

        let config = SecretsConfig {
            location: KeyLocation::Supplied(supplied_path.clone()),
            signs: true,
            ..own(target_ca.path())
        };
        let plan = plan_secrets(&dump, &copy_secrets(), config).await.unwrap();
        let applied = apply_secrets(plan, &repo).await;
        assert!(matches!(
            applied.signing_key,
            KeyOutcome::Installed {
                signs_with: SignsWith::Supplied { ref fingerprint, signs: true },
                ..
            } if fingerprint.as_deref() == Some(supplied_fingerprint.as_str())
        ));
        assert_eq!(std::fs::read(&supplied_path).unwrap(), supplied_file);
        assert_eq!(
            std::fs::read_to_string(target_ca.path().join(KEY_FILE)).unwrap(),
            dump.secrets.unwrap().signing_key.unwrap()
        );
        assert_eq!(repo.key().unwrap().fingerprint(), supplied_fingerprint);
    }

    /// A key supplied at the very path the server keeps its own -- a secret
    /// mounted there -- is the operator's: a restore writes nothing there,
    /// `--clear` removes nothing.
    #[tokio::test]
    async fn a_supplied_key_where_the_server_keeps_its_own_is_never_written() {
        let SourceDump {
            bytes,
            ca: _source_ca,
        } = dump_with_key(SourceKey::Unlocked).await;
        let dump = load_dump(&bytes).unwrap();
        let ca = tempfile::tempdir().unwrap();
        let path = ca.path().join(KEY_FILE);
        let supplied = InstanceKey::load_or_create(&path, None).unwrap();
        let file = std::fs::read(&path).unwrap();
        let repo = Repository::new(tempfile::tempdir().unwrap().keep()).with_key(supplied);
        // Spelled another way, as an operator might.
        let config = || SecretsConfig {
            location: KeyLocation::Supplied(ca.path().join(".").join(KEY_FILE)),
            signs: true,
            ..own(ca.path())
        };

        let plan = plan_secrets(&dump, &copy_secrets(), config())
            .await
            .unwrap();
        let applied = apply_secrets(plan, &repo).await;
        assert!(matches!(applied.signing_key, KeyOutcome::NotWritten { .. }));
        assert_eq!(std::fs::read(&path).unwrap(), file);

        let without = load_dump(&dump_without_secrets().await).unwrap();
        let plan = plan_secrets(&without, &clear(), config()).await.unwrap();
        let applied = apply_secrets(plan, &repo).await;
        assert_eq!(applied.signing_key, KeyOutcome::Kept);
        assert_eq!(std::fs::read(&path).unwrap(), file);
    }

    /// A server that does not sign keeps the restored key for when it does.
    #[tokio::test]
    async fn a_server_that_does_not_sign_keeps_the_restored_key_for_later() {
        let SourceDump {
            bytes,
            ca: _source_ca,
        } = dump_with_key(SourceKey::Unlocked).await;
        let dump = load_dump(&bytes).unwrap();
        let ca = tempfile::tempdir().unwrap();
        let plan = plan_secrets(&dump, &copy_secrets(), own(ca.path()))
            .await
            .unwrap();
        let applied = apply_secrets(plan, &Repository::new(ca.path())).await;
        assert!(matches!(
            applied.signing_key,
            KeyOutcome::Installed {
                signs_with: SignsWith::Nothing,
                ..
            }
        ));
        assert_eq!(applied.ca, CaOutcome::Replaced { unsynced: None });
        assert!(ca.path().join(KEY_FILE).is_file());
    }

    /// Everything a restore writes is written before the rows are, beside
    /// what it replaces: what is left once they have committed is renames,
    /// and when those fail, each secret says so, and neither the files nor
    /// the key the server signs with changed.
    #[cfg(unix)]
    #[tokio::test]
    async fn secrets_that_cannot_be_put_in_place_after_the_commit_say_so() {
        use std::os::unix::fs::PermissionsExt;
        let SourceDump {
            bytes,
            ca: _source_ca,
        } = dump_with_key(SourceKey::Unlocked).await;
        let target = aurcache_db::testing::memory_db().await;
        let target_ca = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let repo = signing_server(&target, target_ca.path(), root.path()).await;
        let ca_key = std::fs::read(target_ca.path().join("ca-key.pem")).unwrap();
        let key_file = std::fs::read(target_ca.path().join(KEY_FILE)).unwrap();
        let fingerprint = repo.key().unwrap().fingerprint().to_string();

        let config = SecretsConfig {
            signs: true,
            ..own(target_ca.path())
        };
        let plan = plan_secrets(&load_dump(&bytes).unwrap(), &copy_secrets(), config)
            .await
            .unwrap();
        // What a full disk or a remount would do between the plan and the
        // renames.
        std::fs::set_permissions(target_ca.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
        if std::fs::File::create(target_ca.path().join("probe")).is_ok() {
            eprintln!("skipped: permissions do not apply here (running as root?)");
            return;
        }
        let applied = apply_secrets(plan, &repo).await;
        std::fs::set_permissions(target_ca.path(), std::fs::Permissions::from_mode(0o755)).unwrap();

        assert!(
            matches!(applied.ca, CaOutcome::Failed { .. }),
            "{applied:?}"
        );
        let KeyOutcome::Failed { error } = &applied.signing_key else {
            panic!("{applied:?}");
        };
        assert!(
            error.contains(&format!("signs with {fingerprint} as before")),
            "{error}"
        );
        assert_eq!(
            std::fs::read(target_ca.path().join("ca-key.pem")).unwrap(),
            ca_key
        );
        assert_eq!(
            std::fs::read(target_ca.path().join(KEY_FILE)).unwrap(),
            key_file
        );
        assert_eq!(repo.key().unwrap().fingerprint(), fingerprint);
    }

    /// `--clear` puts a new key in place of the old by a rename, so there is
    /// never a moment without one, and a key file that is not a regular file
    /// -- one a rename cannot replace -- refuses the restore before anything.
    #[tokio::test]
    async fn clearing_replaces_the_key_file_and_refuses_one_it_cannot_replace() {
        let target = aurcache_db::testing::memory_db().await;
        let target_ca = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let repo = signing_server(&target, target_ca.path(), root.path()).await;
        let before = repo.key().unwrap().fingerprint().to_string();
        let config = || SecretsConfig {
            signs: true,
            ..own(target_ca.path())
        };
        let without = load_dump(&dump_without_secrets().await).unwrap();

        let plan = plan_secrets(&without, &clear(), config()).await.unwrap();
        // Staged: the old key is still there, and still signs.
        let on_disk = std::fs::read(target_ca.path().join(KEY_FILE)).unwrap();
        assert_eq!(
            aurcache_signing::inspect(&on_disk).unwrap().fingerprint,
            before
        );
        let applied = apply_secrets(plan, &repo).await;
        let KeyOutcome::Reset {
            generated: Some(generated),
            supplied: None,
            file,
        } = applied.signing_key
        else {
            panic!("{applied:?}");
        };
        assert_eq!(file.previous.fingerprint(), Some(before.as_str()));
        assert_eq!(repo.key().unwrap().fingerprint(), generated);
        let on_disk = std::fs::read(target_ca.path().join(KEY_FILE)).unwrap();
        assert_eq!(
            aurcache_signing::inspect(&on_disk).unwrap().fingerprint,
            generated
        );
        // The key it replaced is kept beside it, as it was, owner-only.
        let kept = target_ca
            .path()
            .join(format!("{KEY_FILE}.replaced-{before}"));
        assert_eq!(file.previous.kept(), Some(kept.as_path()));
        assert_eq!(
            aurcache_signing::inspect(&std::fs::read(&kept).unwrap())
                .unwrap()
                .fingerprint,
            before
        );
        assert_private(&kept);

        let path = target_ca.path().join(KEY_FILE);
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        let files = listing(target_ca.path());
        let error = plan_secrets(&without, &clear(), config())
            .await
            .err()
            .unwrap();
        assert!(
            format!("{error:#}").contains("AURCACHE_SIGNING_KEY_FILE"),
            "{error:#}"
        );
        assert_eq!(listing(target_ca.path()), files);
    }

    /// A supplied key is recognised where the server keeps its own however
    /// it got there -- a hard link here, a bind mount in a container -- by
    /// being the same file, not by its path.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_supplied_key_linked_where_the_server_keeps_its_own_is_never_written() {
        let SourceDump {
            bytes,
            ca: _source_ca,
        } = dump_with_key(SourceKey::Unlocked).await;
        let dump = load_dump(&bytes).unwrap();
        let ca = tempfile::tempdir().unwrap();
        let secrets = tempfile::tempdir_in(ca.path()).unwrap();
        let supplied_path = secrets.path().join("operator.asc");
        let supplied = InstanceKey::load_or_create(&supplied_path, None).unwrap();
        std::fs::hard_link(&supplied_path, ca.path().join(KEY_FILE)).unwrap();
        let file = std::fs::read(&supplied_path).unwrap();
        let repo = Repository::new(tempfile::tempdir().unwrap().keep()).with_key(supplied);
        let config = SecretsConfig {
            location: KeyLocation::Supplied(supplied_path.clone()),
            signs: true,
            ..own(ca.path())
        };

        let plan = plan_secrets(&dump, &copy_secrets(), config).await.unwrap();
        let applied = apply_secrets(plan, &repo).await;
        assert!(
            matches!(applied.signing_key, KeyOutcome::NotWritten { .. }),
            "{applied:?}"
        );
        assert_eq!(std::fs::read(&supplied_path).unwrap(), file);
        assert_eq!(std::fs::read(ca.path().join(KEY_FILE)).unwrap(), file);
    }

    /// A dump with secrets whose key is `bytes`, a secret key file as an
    /// operator might supply one, of an instance with no packages.
    async fn dump_with_key_file(bytes: &[u8]) -> SourceDump {
        let source = aurcache_db::testing::memory_db().await;
        let ca = tempfile::tempdir().unwrap();
        aurcache_ca::Ca::load_or_create(ca.path()).unwrap();
        std::fs::write(ca.path().join(KEY_FILE), bytes).unwrap();
        let sources = SecretSources { ca_dir: ca.path() };
        let bytes =
            write_archive(&build_dump(&source, "test", Some(sources)).await.unwrap()).unwrap();
        SourceDump { bytes, ca }
    }

    /// An Ed25519 key GnuPG made in 2020 that signed for a day: long expired.
    /// `None` when GnuPG is not there to make one.
    fn expired_gnupg_key(dir: &Path) -> Option<Vec<u8>> {
        let home = dir.join("gnupg");
        std::fs::create_dir(&home).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let gpg = |args: &[&str]| {
            std::process::Command::new("gpg")
                .arg("--homedir")
                .arg(&home)
                .args(["--batch", "--pinentry-mode", "loopback", "--passphrase", ""])
                .args(args)
                .stdin(std::process::Stdio::null())
                .output()
                .ok()
        };
        let made = gpg(&[
            "--faked-system-time",
            "20200101T000000",
            "--quick-gen-key",
            "Expired <ops@localhost>",
            "ed25519",
            "sign",
            "1d",
        ])?;
        let exported = made
            .status
            .success()
            .then(|| gpg(&["--export-secret-keys", "Expired <ops@localhost>"]))
            .flatten();
        let _ = std::process::Command::new("gpgconf")
            .arg("--homedir")
            .arg(&home)
            .args(["--kill", "all"])
            .status();
        exported
            .filter(|out| out.status.success() && !out.stdout.is_empty())
            .map(|out| out.stdout)
    }

    /// A key GnuPG made, and the same key once a subkey that signs was
    /// added to it: the same primary key, another component key signing.
    struct GnupgKeys {
        before: Vec<u8>,
        after: Vec<u8>,
    }

    /// An Ed25519 key GnuPG made, whose primary key signs, before and after
    /// a signing subkey was added to it. `None` when GnuPG is not there to
    /// make one.
    fn gnupg_key_and_its_new_subkey(dir: &Path) -> Option<GnupgKeys> {
        let home = dir.join("gnupg");
        std::fs::create_dir(&home).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let gpg = |args: &[&str]| {
            std::process::Command::new("gpg")
                .arg("--homedir")
                .arg(&home)
                .args(["--batch", "--pinentry-mode", "loopback", "--passphrase", ""])
                .args(args)
                .stdin(std::process::Stdio::null())
                .output()
                .ok()
                .filter(|out| out.status.success())
        };
        let user = "Ops <ops@localhost>";
        let keys = (|| {
            gpg(&["--quick-gen-key", user, "ed25519", "sign", "never"])?;
            let before = gpg(&["--export-secret-keys", user])?.stdout;
            let listed = gpg(&["--with-colons", "--list-secret-keys", user])?.stdout;
            let fingerprint = String::from_utf8(listed)
                .ok()?
                .lines()
                .find_map(|line| line.strip_prefix("fpr:"))
                .map(|line| line.trim_matches(':').to_string())?;
            gpg(&["--quick-add-key", &fingerprint, "ed25519", "sign", "never"])?;
            let after = gpg(&["--export-secret-keys", user])?.stdout;
            Some(GnupgKeys { before, after })
        })();
        let _ = std::process::Command::new("gpgconf")
            .arg("--homedir")
            .arg(&home)
            .args(["--kill", "all"])
            .status();
        keys.filter(|keys| !keys.before.is_empty() && !keys.after.is_empty())
    }

    /// The server's own key restored with a subkey added -- the same
    /// primary key, another component key signing -- is the key that signs
    /// at once, through that subkey: what is on disk is what signs, what the
    /// key signed before is signed again, and the file it replaces is kept.
    #[tokio::test]
    async fn the_same_key_with_a_new_signing_subkey_signs_through_it_at_once() {
        let tmp = tempfile::tempdir().unwrap();
        let Some(GnupgKeys { before, after }) = gnupg_key_and_its_new_subkey(tmp.path()) else {
            eprintln!("skipped: gpg could not make a key");
            return;
        };
        let SourceDump {
            bytes,
            ca: _source_ca,
        } = dump_with_key_file(&after).await;

        let target = aurcache_db::testing::memory_db().await;
        let ca = tempfile::tempdir().unwrap();
        aurcache_ca::Ca::load_or_create(ca.path()).unwrap();
        std::fs::write(ca.path().join(KEY_FILE), &before).unwrap();
        let root = tempfile::tempdir().unwrap();
        let repo = Arc::new(
            Repository::new(root.path())
                .with_key(InstanceKey::load(&ca.path().join(KEY_FILE), None).unwrap()),
        );
        repo.init().unwrap();
        publish(&target, &repo, "hello").await;
        let old = repo.key().unwrap();
        repo.mark_signed(&old);

        let config = SecretsConfig {
            signs: true,
            ..own(ca.path())
        };
        let plan = plan_secrets(&load_dump(&bytes).unwrap(), &copy_secrets(), config)
            .await
            .unwrap();
        let applied = apply_secrets(plan, &repo).await;
        let KeyOutcome::Installed {
            signs_with: SignsWith::NewSigner,
            file,
            ..
        } = &applied.signing_key
        else {
            panic!("{applied:?}");
        };
        let key = repo.key().unwrap();
        assert_eq!(key.fingerprint(), old.fingerprint());
        assert_ne!(key.signer_fingerprint(), old.signer_fingerprint());
        assert!(repo.signing_info().unwrap().backfilling);
        let kept = ca
            .path()
            .join(format!("{KEY_FILE}.replaced-{}", old.fingerprint()));
        assert_eq!(file.previous.kept(), Some(kept.as_path()));
        assert_eq!(std::fs::read(&kept).unwrap(), before);

        assert_eq!(
            backfill(&target, &repo).await.unwrap(),
            Backfill::Complete {
                signed: 1,
                skipped: Vec::new(),
            }
        );
        all_signed_by(&key, root.path());
        let listed =
            pacman_repo_utils::listed_packages(&root.path().join("x86_64/repo.db.tar.gz")).unwrap();
        assert!(key.issued(listed[0].signature.as_deref().unwrap()));
        assert!(!old.issued(listed[0].signature.as_deref().unwrap()));
    }

    /// A restore that writes no key does not read the passphrase file, so
    /// one that has gone missing -- a secret no longer mounted -- does not
    /// refuse a restore of packages alone; one that writes a key reads it,
    /// and is refused, naming it.
    #[tokio::test]
    async fn a_missing_passphrase_file_refuses_only_a_restore_that_writes_a_key() {
        let source = aurcache_db::testing::memory_db().await;
        packages::ActiveModel {
            name: Set("offline".to_string()),
            status: Set(BuildState::Active),
            out_of_date: Set(false),
            build_flags: Set(Default::default()),
            platforms: Set("x86_64".parse().unwrap()),
            source_data: Set(SourceData::Git {
                spec: GitSourceSpec {
                    // Unroutable by construction: the restore needs no network.
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
        let packages_only =
            write_archive(&build_dump(&source, "test", None).await.unwrap()).unwrap();

        let target = aurcache_db::testing::memory_db().await;
        let ca = tempfile::tempdir().unwrap();
        aurcache_ca::Ca::load_or_create(ca.path()).unwrap();
        let missing = tempfile::tempdir().unwrap().keep().join("passphrase");
        let config = || SecretsConfig {
            passphrase_file: Some(missing.clone()),
            ..own(ca.path())
        };
        let entries = restore_with(
            &target,
            config(),
            Arc::new(Repository::new(tempfile::tempdir().unwrap().keep())),
            &packages_only,
            RestoreOptions::default(),
        )
        .await;
        assert!(
            entries.iter().any(
                |entry| entry.pkgbase == "offline" && entry.outcome == RestoreOutcome::Imported
            ),
            "{entries:?}"
        );
        for failure in failures(&entries) {
            assert!(
                !failure.contains("passphrase") && !failure.contains("nothing was imported"),
                "{failure}"
            );
        }
        assert_eq!(
            secret_line(&entries, Secret::SigningKey).change,
            SecretChange::Kept
        );

        let SourceDump {
            bytes,
            ca: _source_ca,
        } = dump_with_key(SourceKey::Unlocked).await;
        let error = plan_secrets(&load_dump(&bytes).unwrap(), &copy_secrets(), config())
            .await
            .err()
            .unwrap();
        assert!(
            format!("{error:#}").contains(&missing.display().to_string()),
            "{error:#}"
        );
    }

    /// A dump whose key has expired since it was taken reads, and restores
    /// what it can: ignoring its secrets, or keeping the key on a server that
    /// does not sign. Only signing with it is refused.
    #[tokio::test]
    async fn a_dump_whose_key_has_expired_restores_what_it_can() {
        let tmp = tempfile::tempdir().unwrap();
        let Some(expired) = expired_gnupg_key(tmp.path()) else {
            eprintln!("skipped: gpg could not make a key");
            return;
        };
        let SourceDump {
            bytes,
            ca: _source_ca,
        } = dump_with_key_file(&expired).await;
        let dump = load_dump(&bytes).unwrap();

        let ca = tempfile::tempdir().unwrap();
        let plan = plan_secrets(&dump, &RestoreOptions::default(), own(ca.path()))
            .await
            .unwrap();
        assert_eq!(
            apply_secrets(plan, &Repository::new(ca.path()))
                .await
                .signing_key,
            KeyOutcome::Kept
        );
        let plan = plan_secrets(&dump, &copy_secrets(), own(ca.path()))
            .await
            .unwrap();
        assert!(matches!(
            apply_secrets(plan, &Repository::new(ca.path()))
                .await
                .signing_key,
            KeyOutcome::Installed {
                signs_with: SignsWith::Nothing,
                ..
            }
        ));

        let signing_ca = tempfile::tempdir().unwrap();
        let config = SecretsConfig {
            signs: true,
            ..own(signing_ca.path())
        };
        let error = plan_secrets(&dump, &copy_secrets(), config)
            .await
            .err()
            .unwrap();
        assert!(
            format!("{error:#}").contains("cannot sign here"),
            "{error:#}"
        );
    }

    /// A key that comes unlocked onto a server that locks its keys is
    /// written locked with its passphrase, whether it signs or is kept.
    #[tokio::test]
    async fn an_unlocked_key_is_written_locked_on_a_server_with_a_passphrase() {
        let SourceDump {
            bytes,
            ca: _source_ca,
        } = dump_with_key(SourceKey::Unlocked).await;
        let dump = load_dump(&bytes).unwrap();

        let signing_ca = tempfile::tempdir().unwrap();
        aurcache_ca::Ca::load_or_create(signing_ca.path()).unwrap();
        let repo = Repository::new(tempfile::tempdir().unwrap().keep())
            .with_key(InstanceKey::generate(Some("correct horse")).unwrap());
        let config = SecretsConfig {
            signs: true,
            ..with_passphrase(signing_ca.path(), "correct horse")
        };
        let plan = plan_secrets(&dump, &copy_secrets(), config).await.unwrap();
        let applied = apply_secrets(plan, &repo).await;
        assert!(
            matches!(
                applied.signing_key,
                KeyOutcome::Installed {
                    signs_with: SignsWith::Restored { .. },
                    ..
                }
            ),
            "{applied:?}"
        );
        let kept_ca = tempfile::tempdir().unwrap();
        let config = with_passphrase(kept_ca.path(), "correct horse");
        let plan = plan_secrets(&dump, &copy_secrets(), config).await.unwrap();
        apply_secrets(plan, &Repository::new(kept_ca.path())).await;

        for ca in [signing_ca.path(), kept_ca.path()] {
            let on_disk = std::fs::read(ca.join(KEY_FILE)).unwrap();
            assert!(aurcache_signing::inspect(&on_disk).unwrap().locked);
            assert!(InstanceKey::load(&ca.join(KEY_FILE), None).is_err());
            InstanceKey::load(&ca.join(KEY_FILE), Some("correct horse")).unwrap();
        }
    }

    /// A key that is kept, not used, is checked against this server's
    /// passphrase only if it has one; a key that is not written is not
    /// checked at all.
    #[tokio::test]
    async fn only_a_key_that_is_written_is_checked_and_against_what_unlocks_it() {
        let SourceDump {
            bytes,
            ca: _source_ca,
        } = dump_with_key(SourceKey::Locked("correct horse")).await;
        let dump = load_dump(&bytes).unwrap();

        let ca = tempfile::tempdir().unwrap();
        let config = with_passphrase(ca.path(), "wrong horse");
        let error = plan_secrets(&dump, &copy_secrets(), config)
            .await
            .err()
            .unwrap();
        assert!(
            format!("{error:#}").contains("does not unlock with"),
            "{error:#}"
        );
        // No passphrase here: kept as it came, locked.
        let plan = plan_secrets(&dump, &copy_secrets(), own(ca.path()))
            .await
            .unwrap();
        apply_secrets(plan, &Repository::new(ca.path())).await;
        assert!(
            aurcache_signing::inspect(&std::fs::read(ca.path().join(KEY_FILE)).unwrap())
                .unwrap()
                .locked
        );

        // Where the server's own file is the one it is supplied, the dump's
        // key is not written, and nothing is asked of it.
        let supplied_ca = tempfile::tempdir().unwrap();
        let path = supplied_ca.path().join(KEY_FILE);
        let supplied = InstanceKey::load_or_create(&path, None).unwrap();
        let config = SecretsConfig {
            location: KeyLocation::Supplied(path.clone()),
            signs: true,
            ..own(supplied_ca.path())
        };
        let plan = plan_secrets(&dump, &copy_secrets(), config).await.unwrap();
        let repo = Repository::new(tempfile::tempdir().unwrap().keep()).with_key(supplied);
        assert!(matches!(
            apply_secrets(plan, &repo).await.signing_key,
            KeyOutcome::NotWritten { signs: true, .. }
        ));
    }

    /// `--clear` with nothing to clear writes nothing, so a CA directory
    /// mounted read-only does not refuse it.
    #[cfg(unix)]
    #[tokio::test]
    async fn clearing_with_nothing_to_clear_writes_nothing() {
        use std::os::unix::fs::PermissionsExt;
        let ca = tempfile::tempdir().unwrap();
        let supplied_dir = tempfile::tempdir().unwrap();
        let supplied = supplied_dir.path().join("operator.asc");
        InstanceKey::load_or_create(&supplied, None).unwrap();
        std::fs::set_permissions(ca.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
        if std::fs::File::create(ca.path().join("probe")).is_ok() {
            eprintln!("skipped: permissions do not apply here (running as root?)");
            return;
        }
        let config = SecretsConfig {
            location: KeyLocation::Supplied(supplied),
            signs: true,
            ..own(ca.path())
        };
        let without = load_dump(&dump_without_secrets().await).unwrap();
        let planned = plan_secrets(&without, &clear(), config).await;
        std::fs::set_permissions(ca.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        planned.unwrap();
    }

    /// Rows that committed are imported, even when the repository's files
    /// could not all be put in place after: the restore says so, goes on,
    /// and puts the secrets in place -- it is not "nothing was imported".
    #[tokio::test]
    async fn rows_committed_before_the_files_were_in_place_are_imported() {
        let target = aurcache_db::testing::memory_db().await;
        let target_ca = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let repo = signing_server(&target, target_ca.path(), root.path()).await;
        let before = repo.key().unwrap().fingerprint().to_string();
        // Recorded as published, so `--clear` retires it and the update
        // rewrites the databases.
        let package_id = package(&target, "hello", true, None).await;
        aurcache_db::files::ActiveModel {
            filename: Set("hello-1.0-1-x86_64.pkg.tar.zst".to_string()),
            platform: Set(Platform::X86_64),
            package_id: Set(package_id),
            ..Default::default()
        }
        .insert(&target)
        .await
        .unwrap();
        // The database's signature cannot be renamed into place.
        let blocked = root.path().join("x86_64").join("repo.db.tar.gz.sig");
        std::fs::remove_file(&blocked).unwrap();
        std::fs::create_dir(&blocked).unwrap();
        std::fs::write(blocked.join("in-the-way"), b"x").unwrap();

        let entries = restore(
            &target,
            target_ca.path(),
            Arc::clone(&repo),
            &dump_without_secrets().await,
            clear(),
        )
        .await;
        let failures = failures(&entries);
        assert!(
            failures
                .iter()
                .all(|error| !error.contains("nothing was imported")),
            "{entries:?}"
        );
        assert!(
            failures
                .iter()
                .any(|error| error.contains("not all in place yet")),
            "{entries:?}"
        );
        assert_ne!(
            repo.key().unwrap().fingerprint(),
            before,
            "the secrets were put in place after the rows"
        );

        std::fs::remove_dir_all(&blocked).unwrap();
        repo.recover(&target).await.unwrap();
        assert!(blocked.is_file());
    }
}
