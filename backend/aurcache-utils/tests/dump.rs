//! What a dump carries, and what it deliberately leaves behind.

use aurcache_activitylog::activity_utils::ActivityLog;
use aurcache_common::api::dump::{
    DUMP_SCHEMA_VERSION, KnownHostsChange, MANIFEST_FILE, PACKAGES_FILE, SshKeyChange,
};
use aurcache_common::build_state::BuildState;
use aurcache_common::source::GitSourceSpec;
use aurcache_db::packages::SourceData;
use aurcache_db::prelude::Packages;
use aurcache_db::{packages, settings, workers};
use aurcache_utils::dump::{SecretSources, build_dump, write_archive};
use aurcache_utils::services::Services;
use aurcache_utils::ssh::{HostKeySource, KeySource, SshAccess, SshConfig};
use flate2::read::GzDecoder;
use sea_orm::{ActiveModelTrait, DatabaseConnection, EntityTrait, Set};
use std::collections::HashMap;
use std::io::Read;
use std::sync::Arc;

/// A repository of its own for a restore that publishes nothing new.
fn test_repo() -> aurcache_utils::repository::Repository {
    aurcache_utils::repository::Repository::new(tempfile::tempdir().unwrap().keep())
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
/// No SSH configuration, for tests about everything else.
fn no_ssh() -> &'static Arc<SshAccess> {
    static NO_SSH: std::sync::LazyLock<Arc<SshAccess>> =
        std::sync::LazyLock::new(|| Arc::new(SshAccess::unavailable("not under test")));
    &NO_SSH
}

/// Secrets from the CA at `ca_dir`, and no SSH configuration.
fn without_ssh(ca_dir: &std::path::Path) -> SecretSources<'_> {
    SecretSources {
        ca_dir,
        ssh: no_ssh(),
    }
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
    let entries = preview(&target, no_ssh(), &loaded, &RestoreOptions::default())
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
        no_ssh(),
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

    let dump = build_dump(&source, "test", Some(without_ssh(ca_dir.path())))
        .await
        .unwrap();
    assert!(dump.manifest.includes_secrets);
    let secrets = dump.secrets.clone().unwrap();
    assert_eq!(secrets.ca_key_pem, ca.private_key_pem());
    assert_eq!(secrets.tokens.len(), 1);
    // The certificate travels only in this mode.
    assert!(dump.workers[0].signed_cert.is_some());

    let files = archive_files(&write_archive(&dump).unwrap());
    assert_eq!(
        files.get("ca-key.pem").map(String::as_str),
        Some(ca.private_key_pem())
    );

    // And it round-trips through the reader, which is what a restore does.
    let loaded = load_dump(&write_archive(&dump).unwrap()).unwrap();
    assert_eq!(loaded.secrets.unwrap().ca_key_pem, ca.private_key_pem());
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
        &build_dump(&source, "test", Some(without_ssh(src_ca.path())))
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
        target_ca.path(),
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
        &build_dump(&source, "test", Some(without_ssh(src_ca.path())))
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
        target_ca.path(),
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
        no_ssh(),
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

// ---------------------------------------------------------------------------
// The server's SSH state in dumps.
// ---------------------------------------------------------------------------

const GITHUB_ED25519: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl";

/// A server's SSH directory with a generated key and one host recorded.
fn ssh_server(dir: &std::path::Path) -> Arc<SshAccess> {
    let ssh = SshAccess::new(SshConfig {
        dir: dir.to_path_buf(),
        key: KeySource::Generated(dir.join("id_ed25519")),
        passphrase_file: None,
        hosts: HostKeySource::TrustOnFirstUse(dir.join("known_hosts")),
    });
    std::fs::write(
        dir.join("known_hosts"),
        format!("github.com {GITHUB_ED25519}\n"),
    )
    .unwrap();
    Arc::new(ssh)
}

fn fingerprint(ssh: &SshAccess) -> String {
    match ssh.info().key {
        aurcache_common::api::ssh::SshKey::Ready { fingerprint, .. } => fingerprint,
        other => panic!("{other:?}"),
    }
}

/// A key protected by `passphrase`, made by ssh-keygen; `None` without it.
fn protected_key(path: &std::path::Path, passphrase: &str) -> Option<()> {
    let made = std::process::Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", passphrase, "-f"])
        .arg(path)
        .status();
    if made.is_ok_and(|s| s.success()) {
        Some(())
    } else {
        eprintln!("skipped: ssh-keygen is not installed");
        None
    }
}

/// Without secrets a dump has nothing of SSH in it, so it restores anywhere
/// and asks for no key.
#[tokio::test]
async fn a_dump_without_secrets_carries_nothing_of_ssh() {
    let source = aurcache_db::testing::memory_db().await;
    package(&source, "hello", true, None).await;
    let ssh_dir = tempfile::tempdir().unwrap();
    let _ssh = ssh_server(ssh_dir.path());

    let dump = build_dump(&source, "test", None).await.unwrap();
    let files = archive_files(&write_archive(&dump).unwrap());
    assert!(
        files.keys().all(|name| !name.starts_with("ssh/")),
        "{:?}",
        files.keys()
    );
}

/// With secrets, the key travels byte for byte -- an encrypted key stays
/// encrypted -- with the host keys trusted on first use, and never the
/// passphrase.
#[tokio::test]
async fn a_dump_with_secrets_carries_the_key_as_stored_and_the_trusted_hosts() {
    let ssh_dir = tempfile::tempdir().unwrap();
    // A supplied key, protected by a passphrase.
    let key = ssh_dir.path().join("deploy_key");
    if protected_key(&key, "correct horse").is_none() {
        return;
    }
    let passphrase = ssh_dir.path().join("passphrase");
    std::fs::write(&passphrase, "correct horse\n").unwrap();
    std::fs::write(
        ssh_dir.path().join("known_hosts"),
        "example.com ssh-ed25519 AAAA\n",
    )
    .unwrap();
    let ssh = Arc::new(SshAccess::new(SshConfig {
        dir: ssh_dir.path().to_path_buf(),
        key: KeySource::Provided(key.clone()),
        passphrase_file: Some(passphrase),
        hosts: HostKeySource::TrustOnFirstUse(ssh_dir.path().join("known_hosts")),
    }));

    let bytes = dump_with_ssh(&ssh).await;
    let files = archive_files(&bytes);
    assert_eq!(
        files.get("ssh/key").map(String::as_str),
        Some(std::fs::read_to_string(&key).unwrap().as_str()),
        "the key was not carried as stored"
    );
    assert!(files["ssh/key"].contains("BEGIN OPENSSH PRIVATE KEY"));
    assert!(files["ssh/key.pub"].starts_with("ssh-ed25519 "));
    assert_eq!(files["ssh/known_hosts"], "example.com ssh-ed25519 AAAA\n");
    for (name, content) in &files {
        assert!(
            !content.contains("correct horse"),
            "the passphrase is in {name}"
        );
    }
    let loaded = load_dump(&bytes).unwrap();
    let carried = loaded.secrets.unwrap().ssh;
    assert_eq!(carried.key.unwrap().private_key, files["ssh/key"]);
}

/// The secrets in an archive are written for their owner only.
#[tokio::test]
async fn secrets_are_owner_only_in_the_archive() {
    let ssh_dir = tempfile::tempdir().unwrap();
    let bytes = dump_with_ssh(&ssh_server(ssh_dir.path())).await;
    let mut archive = tar::Archive::new(GzDecoder::new(&bytes[..]));
    for entry in archive.entries().unwrap() {
        let entry = entry.unwrap();
        let path = entry.path().unwrap().to_string_lossy().into_owned();
        let mode = entry.header().mode().unwrap();
        if ["ca-key.pem", "tokens.json", "ssh/key"].contains(&path.as_str()) {
            assert_eq!(mode & 0o077, 0, "{path} is mode {mode:o}");
        }
    }
}

/// A file the operator pins is theirs: it is not carried.
#[tokio::test]
async fn pinned_host_keys_are_not_carried() {
    let ssh_dir = tempfile::tempdir().unwrap();
    let pinned = ssh_dir.path().join("pinned");
    std::fs::write(&pinned, "example.com ssh-ed25519 AAAA\n").unwrap();
    let ssh = Arc::new(SshAccess::new(SshConfig {
        dir: ssh_dir.path().to_path_buf(),
        key: KeySource::Generated(ssh_dir.path().join("id_ed25519")),
        passphrase_file: None,
        hosts: HostKeySource::Pinned(pinned),
    }));
    let files = archive_files(&dump_with_ssh(&ssh).await);
    assert!(files.contains_key("ssh/key"));
    assert!(!files.contains_key("ssh/known_hosts"));
}

/// SSH files outside a dump with secrets mean it was assembled wrongly.
#[test]
fn ssh_files_without_the_rest_of_the_secrets_are_refused() {
    let manifest = serde_json::json!({
        "schema_version": DUMP_SCHEMA_VERSION,
        "aurcache_version": "test",
        "created_at": 0,
        "includes_secrets": false,
    })
    .to_string();
    let mut bytes = Vec::new();
    {
        let mut tar = tar::Builder::new(flate2::write::GzEncoder::new(
            &mut bytes,
            flate2::Compression::default(),
        ));
        for (path, content) in [
            (MANIFEST_FILE, manifest.as_str()),
            (PACKAGES_FILE, "{}"),
            ("settings.json", r#"{"global": {}, "packages": {}}"#),
            ("workers.json", "[]"),
            ("ssh/key", "KEY"),
        ] {
            let mut header = tar::Header::new_gnu();
            header.set_path(path).unwrap();
            header.set_size(content.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            tar.append(&header, content.as_bytes()).unwrap();
        }
        tar.into_inner().unwrap().finish().unwrap();
    }
    let error = load_dump(&bytes).unwrap_err().to_string();
    assert!(error.contains("ssh/key"), "{error}");
}

/// A dump of `ssh` with secrets, carrying one package.
async fn dump_with_ssh(ssh: &Arc<SshAccess>) -> Vec<u8> {
    let source = aurcache_db::testing::memory_db().await;
    package(&source, "hello", true, None).await;
    let ca = tempfile::tempdir().unwrap();
    aurcache_ca::Ca::load_or_create(ca.path()).unwrap();
    write_archive(
        &build_dump(
            &source,
            "test",
            Some(SecretSources {
                ca_dir: ca.path(),
                ssh,
            }),
        )
        .await
        .unwrap(),
    )
    .unwrap()
}

/// What restoring a dump left behind.
struct Restored {
    db: DatabaseConnection,
    store: Arc<aurcache_utils::snapshot::SnapshotStore>,
    failures: Vec<String>,
    /// What the restore said of the SSH key, and of the trusted host keys.
    key: Option<SshKeyChange>,
    known_hosts: Option<KnownHostsChange>,
}

/// Restore `bytes` into a fresh database with `ssh` as the server's SSH.
async fn restore_with_ssh(bytes: &[u8], ssh: Arc<SshAccess>, options: RestoreOptions) -> Restored {
    let target = aurcache_db::testing::memory_db().await;
    let target_ca = tempfile::tempdir().unwrap();
    let store = Arc::new(
        aurcache_utils::snapshot::SnapshotStore::with_checkout_root(
            tempfile::tempdir().unwrap().keep(),
        )
        .with_ssh(ssh),
    );
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
        target_ca.path(),
        load_dump(bytes).unwrap(),
        options,
        progress_tx,
    )
    .await;
    let mut failures = Vec::new();
    let mut key = None;
    let mut known_hosts = None;
    while let Ok(entry) = progress_rx.try_recv() {
        match entry.outcome {
            RestoreOutcome::Failed { error } => failures.push(error),
            RestoreOutcome::SshKey { change } => key = Some(change),
            RestoreOutcome::SshKnownHosts { change } => known_hosts = Some(change),
            _ => {}
        }
    }
    Restored {
        db: target,
        store,
        failures,
        key,
        known_hosts,
    }
}

fn copying() -> RestoreOptions {
    RestoreOptions {
        secrets: aurcache_common::api::dump::SecretsPolicy::Copy,
        ..RestoreOptions::default()
    }
}

fn clearing() -> RestoreOptions {
    RestoreOptions {
        clear: true,
        ..RestoreOptions::default()
    }
}

/// A migration onto a new instance keeps the key the remotes know: the one
/// the new server generated is replaced, and used at once. The key it
/// replaced is kept beside it, private, named for its fingerprint.
#[tokio::test]
async fn restoring_secrets_replaces_a_freshly_generated_key() {
    let old_dir = tempfile::tempdir().unwrap();
    let old = ssh_server(old_dir.path());
    let bytes = dump_with_ssh(&old).await;

    let new_dir = tempfile::tempdir().unwrap();
    let new = Arc::new(SshAccess::new(SshConfig {
        dir: new_dir.path().to_path_buf(),
        key: KeySource::Generated(new_dir.path().join("id_ed25519")),
        passphrase_file: None,
        hosts: HostKeySource::TrustOnFirstUse(new_dir.path().join("known_hosts")),
    }));
    assert_ne!(fingerprint(&old), fingerprint(&new));
    let generated = std::fs::read(new_dir.path().join("id_ed25519")).unwrap();
    let kept = new_dir.path().join(format!(
        "id_ed25519.replaced-{}",
        fingerprint(&new)
            .replacen("SHA256:", "SHA256-", 1)
            .replace('/', "_")
            .replace('+', "-")
    ));

    // The dry run says what will happen, and the restore what did: the same.
    let expected = SshKeyChange::Installed {
        fingerprint: fingerprint(&old),
        previous: Some(kept.display().to_string()),
    };
    let target = aurcache_db::testing::memory_db().await;
    let previewed = preview(&target, &new, &load_dump(&bytes).unwrap(), &copying())
        .await
        .unwrap();
    assert!(
        previewed.iter().any(|e| e.pkgbase.is_empty()
            && e.outcome
                == RestoreOutcome::SshKey {
                    change: expected.clone()
                }),
        "{previewed:?}"
    );
    assert!(
        previewed.iter().any(|e| matches!(
            &e.outcome,
            RestoreOutcome::SshKnownHosts {
                change: KnownHostsChange::Merged { .. }
            }
        )),
        "{previewed:?}"
    );

    let restored = restore_with_ssh(&bytes, new, copying()).await;
    assert!(restored.failures.is_empty(), "{:?}", restored.failures);
    assert_eq!(restored.key, Some(expected));
    assert_eq!(
        fingerprint(restored.store.ssh()),
        fingerprint(&old),
        "not in use"
    );
    assert_eq!(
        std::fs::read(new_dir.path().join("id_ed25519")).unwrap(),
        std::fs::read(old_dir.path().join("id_ed25519")).unwrap()
    );
    assert_eq!(
        std::fs::read_to_string(new_dir.path().join("known_hosts")).unwrap(),
        std::fs::read_to_string(old_dir.path().join("known_hosts")).unwrap()
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(new_dir.path().join("id_ed25519"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0, "the restored key is readable by others");
        let mode = std::fs::metadata(&kept).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "the replaced key is not private");
    }
    assert_eq!(
        std::fs::read(&kept).unwrap(),
        generated,
        "the replaced key is lost"
    );
}

/// A default restore leaves secrets alone, the SSH key with them.
#[tokio::test]
async fn a_default_restore_leaves_the_ssh_key_alone() {
    let old_dir = tempfile::tempdir().unwrap();
    let bytes = dump_with_ssh(&ssh_server(old_dir.path())).await;
    let new_dir = tempfile::tempdir().unwrap();
    let new = ssh_server(new_dir.path());
    let before = std::fs::read(new_dir.path().join("id_ed25519")).unwrap();
    let restored = restore_with_ssh(&bytes, new, RestoreOptions::default()).await;
    assert_eq!(
        std::fs::read(new_dir.path().join("id_ed25519")).unwrap(),
        before
    );
    assert!(restored.failures.is_empty(), "{:?}", restored.failures);
}

/// A key this server cannot open is refused before anything is touched:
/// no package is imported, and the working key stays.
#[tokio::test]
async fn a_key_that_cannot_be_opened_refuses_the_whole_restore() {
    let old_dir = tempfile::tempdir().unwrap();
    let key = old_dir.path().join("deploy_key");
    if protected_key(&key, "correct horse").is_none() {
        return;
    }
    let passphrase = old_dir.path().join("passphrase");
    std::fs::write(&passphrase, "correct horse\n").unwrap();
    let old = Arc::new(SshAccess::new(SshConfig {
        dir: old_dir.path().to_path_buf(),
        key: KeySource::Provided(key),
        passphrase_file: Some(passphrase),
        hosts: HostKeySource::TrustOnFirstUse(old_dir.path().join("known_hosts")),
    }));
    let bytes = dump_with_ssh(&old).await;

    // No passphrase file here.
    let new_dir = tempfile::tempdir().unwrap();
    let new = ssh_server(new_dir.path());
    let before = std::fs::read(new_dir.path().join("id_ed25519")).unwrap();
    let in_use = fingerprint(&new);

    let loaded = load_dump(&bytes).unwrap();
    let target = aurcache_db::testing::memory_db().await;
    let previewed = preview(&target, &new, &loaded, &copying()).await.unwrap();
    assert!(
        previewed.iter().any(|e| matches!(&e.outcome,
            RestoreOutcome::Failed { error } if error.contains("nothing would be imported")
                && error.contains("passphrase"))),
        "{previewed:?}"
    );

    let restored = restore_with_ssh(&bytes, new, copying()).await;
    assert!(
        restored
            .failures
            .iter()
            .any(|e| e.contains("nothing was imported") && e.contains("passphrase")),
        "{:?}",
        restored.failures
    );
    assert!(
        Packages::find().all(&restored.db).await.unwrap().is_empty(),
        "a refused restore imported packages"
    );
    assert_eq!(
        std::fs::read(new_dir.path().join("id_ed25519")).unwrap(),
        before
    );
    assert_eq!(fingerprint(restored.store.ssh()), in_use);
}

/// The operator's key is never written, even where it is the server's own
/// key file; pinned host keys neither.
#[tokio::test]
async fn an_operators_key_and_pinned_hosts_are_never_written() {
    let old_dir = tempfile::tempdir().unwrap();
    let old = ssh_server(old_dir.path());
    let bytes = dump_with_ssh(&old).await;

    for at_own_path in [false, true] {
        let new_dir = tempfile::tempdir().unwrap();
        let own_dir = new_dir.path().join("ssh");
        // Any key that is not the dump's.
        let other = ssh_server(&new_dir.path().join("other"));
        drop(other);
        let operator_key = if at_own_path {
            std::fs::create_dir_all(&own_dir).unwrap();
            own_dir.join("id_ed25519")
        } else {
            new_dir.path().join("operator_key")
        };
        std::fs::copy(new_dir.path().join("other/id_ed25519"), &operator_key).unwrap();
        let operator_bytes = std::fs::read(&operator_key).unwrap();
        let pinned = new_dir.path().join("pinned");
        std::fs::write(&pinned, "mine\n").unwrap();
        let new = Arc::new(SshAccess::new(SshConfig {
            dir: own_dir.clone(),
            key: KeySource::Provided(operator_key.clone()),
            passphrase_file: None,
            hosts: HostKeySource::Pinned(pinned.clone()),
        }));
        let in_use = fingerprint(&new);

        let restored = restore_with_ssh(&bytes, new, copying()).await;
        assert!(restored.failures.is_empty(), "{:?}", restored.failures);
        assert_eq!(
            std::fs::read(&operator_key).unwrap(),
            operator_bytes,
            "the operator's key was written (at its own path: {at_own_path})"
        );
        assert_eq!(std::fs::read_to_string(&pinned).unwrap(), "mine\n");
        assert_eq!(fingerprint(restored.store.ssh()), in_use);
        if !at_own_path {
            assert_eq!(
                std::fs::read(own_dir.join("id_ed25519")).unwrap(),
                std::fs::read(old_dir.path().join("id_ed25519")).unwrap(),
                "the dump's key is not in the server's own key file"
            );
        }
    }
}

/// On the server a dump was taken from, the dump's key is the operator's,
/// in use already: a restore writes no second copy of it into the server's
/// own key file.
#[tokio::test]
async fn an_operators_key_in_the_dump_is_not_copied_beside_itself() {
    let dir = tempfile::tempdir().unwrap();
    let other = ssh_server(&dir.path().join("other"));
    drop(other);
    let operator_key = dir.path().join("operator_key");
    std::fs::copy(dir.path().join("other/id_ed25519"), &operator_key).unwrap();
    let own_dir = dir.path().join("ssh");
    let server = || {
        Arc::new(SshAccess::new(SshConfig {
            dir: own_dir.clone(),
            key: KeySource::Provided(operator_key.clone()),
            passphrase_file: None,
            hosts: HostKeySource::TrustOnFirstUse(own_dir.join("known_hosts")),
        }))
    };
    let bytes = dump_with_ssh(&server()).await;
    let restored = restore_with_ssh(&bytes, server(), copying()).await;
    assert!(restored.failures.is_empty(), "{:?}", restored.failures);
    assert!(
        !own_dir.join("id_ed25519").exists(),
        "a second copy of the operator's key was written"
    );
}

/// A key file a rename cannot replace -- a symlink, or a Docker secret
/// bind-mounted at the server's own key file -- refuses the restore before
/// anything is written, rather than failing after the packages are in. A
/// bind mount needs a mount namespace (`unshare -rm`); without one only the
/// symlink is tried.
#[tokio::test]
async fn a_key_file_that_cannot_be_replaced_refuses_the_restore() {
    let old_dir = tempfile::tempdir().unwrap();
    let bytes = dump_with_ssh(&ssh_server(old_dir.path())).await;

    for mounted in [false, true] {
        let new_dir = tempfile::tempdir().unwrap();
        let elsewhere = new_dir.path().join("elsewhere");
        let other = ssh_server(&elsewhere);
        drop(other);
        let own_dir = new_dir.path().join("ssh");
        std::fs::create_dir_all(&own_dir).unwrap();
        let own = own_dir.join("id_ed25519");
        if mounted {
            std::fs::write(&own, "").unwrap();
            let bound = std::process::Command::new("mount")
                .arg("--bind")
                .arg(elsewhere.join("id_ed25519"))
                .arg(&own)
                .stderr(std::process::Stdio::null())
                .status();
            if !bound.is_ok_and(|status| status.success()) {
                eprintln!("skipped the bind mount: cannot mount here");
                continue;
            }
        } else {
            std::os::unix::fs::symlink(elsewhere.join("id_ed25519"), &own).unwrap();
        }
        let new = Arc::new(SshAccess::new(SshConfig {
            dir: own_dir.clone(),
            key: KeySource::Generated(own.clone()),
            passphrase_file: None,
            hosts: HostKeySource::TrustOnFirstUse(own_dir.join("known_hosts")),
        }));
        let in_use = fingerprint(&new);
        let restored = restore_with_ssh(&bytes, new, copying()).await;
        if mounted {
            let _ = std::process::Command::new("umount").arg(&own).status();
        }
        let expected = if mounted {
            "is mounted where it is"
        } else {
            "is not a regular file"
        };
        assert!(
            restored
                .failures
                .iter()
                .any(|e| e.contains("nothing was imported") && e.contains(expected)),
            "mounted: {mounted}: {:?}",
            restored.failures
        );
        assert!(Packages::find().all(&restored.db).await.unwrap().is_empty());
        assert_eq!(fingerprint(restored.store.ssh()), in_use);
    }
}

/// `--clear` with a dump that carries no SSH state keeps this server's, as
/// it keeps the CA: the key the remotes know, and the hosts it trusts.
#[tokio::test]
async fn clearing_without_secrets_keeps_the_ssh_state() {
    let source = aurcache_db::testing::memory_db().await;
    package(&source, "hello", true, None).await;
    let bytes = write_archive(&build_dump(&source, "test", None).await.unwrap()).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let ssh = ssh_server(dir.path());
    let before = fingerprint(&ssh);
    let key = std::fs::read(dir.path().join("id_ed25519")).unwrap();
    let hosts = std::fs::read_to_string(dir.path().join("known_hosts")).unwrap();
    let restored = restore_with_ssh(&bytes, ssh, clearing()).await;
    assert!(restored.failures.is_empty(), "{:?}", restored.failures);
    assert_eq!(restored.key, Some(SshKeyChange::Kept));
    assert_eq!(restored.known_hosts, Some(KnownHostsChange::Kept));
    assert_eq!(fingerprint(restored.store.ssh()), before);
    assert_eq!(std::fs::read(dir.path().join("id_ed25519")).unwrap(), key);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("known_hosts")).unwrap(),
        hosts
    );
}

/// Without `--clear`, trusted hosts are merged: none this server recorded is
/// lost, and a dump that trusts another key for one of them is refused.
#[tokio::test]
async fn trusted_hosts_are_merged_and_a_conflict_is_refused() {
    let old_dir = tempfile::tempdir().unwrap();
    let old = ssh_server(old_dir.path());
    std::fs::write(
        old_dir.path().join("known_hosts"),
        format!("github.com {GITHUB_ED25519}\nold.example.com {GITHUB_ED25519}\n"),
    )
    .unwrap();
    let bytes = dump_with_ssh(&old).await;

    let new_dir = tempfile::tempdir().unwrap();
    let new = ssh_server(new_dir.path());
    std::fs::write(
        new_dir.path().join("known_hosts"),
        format!("new.example.com {GITHUB_ED25519}\n"),
    )
    .unwrap();
    let restored = restore_with_ssh(&bytes, new, copying()).await;
    assert!(restored.failures.is_empty(), "{:?}", restored.failures);
    let merged = std::fs::read_to_string(new_dir.path().join("known_hosts")).unwrap();
    for host in ["new.example.com", "old.example.com", "github.com"] {
        assert!(merged.contains(host), "{host} missing from {merged}");
    }

    // This server trusts another key for github.com.
    let other_dir = tempfile::tempdir().unwrap();
    let other = ssh_server(other_dir.path());
    let other_key = std::fs::read_to_string(other_dir.path().join("id_ed25519.pub")).unwrap();
    let mine = format!("github.com {}\n", other_key.trim());
    std::fs::write(other_dir.path().join("known_hosts"), &mine).unwrap();
    let restored = restore_with_ssh(&bytes, other, copying()).await;
    assert!(
        restored
            .failures
            .iter()
            .any(|e| e.contains("nothing was imported") && e.contains("github.com")),
        "{:?}",
        restored.failures
    );
    assert_eq!(
        std::fs::read_to_string(other_dir.path().join("known_hosts")).unwrap(),
        mine
    );
}

/// A dump whose SSH key is not a key at all is refused before anything is
/// touched, rather than taken on its `.pub`'s word.
#[tokio::test]
async fn a_dump_key_that_is_no_key_refuses_the_restore() {
    let old_dir = tempfile::tempdir().unwrap();
    let old = ssh_server(old_dir.path());
    let public = std::fs::read_to_string(old_dir.path().join("id_ed25519.pub")).unwrap();
    // The private key replaced by something that is not one.
    std::fs::write(old_dir.path().join("id_ed25519"), "not a key\n").unwrap();
    let bytes = dump_with_ssh(&old).await;
    assert!(archive_files(&bytes)["ssh/key.pub"].contains(public.trim()));

    let new_dir = tempfile::tempdir().unwrap();
    let new = ssh_server(new_dir.path());
    let before = std::fs::read(new_dir.path().join("id_ed25519")).unwrap();
    let restored = restore_with_ssh(&bytes, new, copying()).await;
    assert!(
        restored
            .failures
            .iter()
            .any(|e| e.contains("nothing was imported") && e.contains("older format")),
        "{:?}",
        restored.failures
    );
    assert_eq!(
        std::fs::read(new_dir.path().join("id_ed25519")).unwrap(),
        before
    );
    assert!(Packages::find().all(&restored.db).await.unwrap().is_empty());
}

/// A refused dry run lists the refusal and nothing it would have imported.
#[tokio::test]
async fn a_refused_dry_run_lists_only_the_refusal() {
    let old_dir = tempfile::tempdir().unwrap();
    let old = ssh_server(old_dir.path());
    std::fs::write(old_dir.path().join("id_ed25519"), "not a key\n").unwrap();
    let loaded = load_dump(&dump_with_ssh(&old).await).unwrap();
    let new_dir = tempfile::tempdir().unwrap();
    let target = aurcache_db::testing::memory_db().await;
    let entries = preview(&target, &ssh_server(new_dir.path()), &loaded, &copying())
        .await
        .unwrap();
    assert_eq!(entries.len(), 1, "{entries:?}");
    assert!(entries[0].pkgbase.is_empty());
}

/// A wildcard entry cannot vouch for another key for a host this server
/// already knows.
#[tokio::test]
async fn a_wildcard_entry_cannot_override_a_known_host() {
    let old_dir = tempfile::tempdir().unwrap();
    let old = ssh_server(old_dir.path());
    let other_dir = tempfile::tempdir().unwrap();
    let _other = ssh_server(other_dir.path());
    let other_key = std::fs::read_to_string(other_dir.path().join("id_ed25519.pub")).unwrap();
    std::fs::write(
        old_dir.path().join("known_hosts"),
        format!("* {}\n", other_key.trim()),
    )
    .unwrap();
    let bytes = dump_with_ssh(&old).await;

    let new_dir = tempfile::tempdir().unwrap();
    let new = ssh_server(new_dir.path());
    let mine = std::fs::read_to_string(new_dir.path().join("known_hosts")).unwrap();
    let restored = restore_with_ssh(&bytes, new, copying()).await;
    assert!(
        restored
            .failures
            .iter()
            .any(|e| e.contains("nothing was imported") && e.contains("github.com")),
        "{:?}",
        restored.failures
    );
    assert_eq!(
        std::fs::read_to_string(new_dir.path().join("known_hosts")).unwrap(),
        mine
    );
}

/// What will not be written is not checked: a protected dump key that would
/// land on the operator's own key file needs no passphrase here, and one
/// beside it, unused, needs none when this server has none.
#[tokio::test]
async fn a_key_that_will_not_be_used_needs_no_passphrase() {
    let old_dir = tempfile::tempdir().unwrap();
    let key = old_dir.path().join("deploy_key");
    if protected_key(&key, "correct horse").is_none() {
        return;
    }
    let passphrase = old_dir.path().join("passphrase");
    std::fs::write(&passphrase, "correct horse\n").unwrap();
    let old = Arc::new(SshAccess::new(SshConfig {
        dir: old_dir.path().to_path_buf(),
        key: KeySource::Provided(key),
        passphrase_file: Some(passphrase),
        hosts: HostKeySource::TrustOnFirstUse(old_dir.path().join("known_hosts")),
    }));
    let bytes = dump_with_ssh(&old).await;

    for at_own_path in [true, false] {
        let new_dir = tempfile::tempdir().unwrap();
        let own_dir = new_dir.path().join("ssh");
        let other = ssh_server(&new_dir.path().join("other"));
        drop(other);
        let operator_key = if at_own_path {
            std::fs::create_dir_all(&own_dir).unwrap();
            own_dir.join("id_ed25519")
        } else {
            new_dir.path().join("operator_key")
        };
        std::fs::copy(new_dir.path().join("other/id_ed25519"), &operator_key).unwrap();
        let new = Arc::new(SshAccess::new(SshConfig {
            dir: own_dir.clone(),
            key: KeySource::Provided(operator_key),
            passphrase_file: None,
            hosts: HostKeySource::TrustOnFirstUse(own_dir.join("known_hosts")),
        }));
        let restored = restore_with_ssh(&bytes, new, copying()).await;
        assert!(
            restored.failures.is_empty(),
            "at its own path: {at_own_path}: {:?}",
            restored.failures
        );
    }
}

/// `--clear` with a key in the dump: the dump's key, in use at once, and
/// the dump's trusted hosts and nothing else. The hosts it replaced are kept
/// beside them.
#[tokio::test]
async fn clearing_with_a_carried_key_installs_it() {
    let old_dir = tempfile::tempdir().unwrap();
    let old = ssh_server(old_dir.path());
    let bytes = dump_with_ssh(&old).await;

    let new_dir = tempfile::tempdir().unwrap();
    let new = ssh_server(new_dir.path());
    std::fs::write(
        new_dir.path().join("known_hosts"),
        format!("only.here.example.com {GITHUB_ED25519}\n"),
    )
    .unwrap();
    let restored = restore_with_ssh(&bytes, new, clearing()).await;
    assert!(restored.failures.is_empty(), "{:?}", restored.failures);
    assert_eq!(fingerprint(restored.store.ssh()), fingerprint(&old));
    assert_eq!(
        std::fs::read_to_string(new_dir.path().join("known_hosts")).unwrap(),
        std::fs::read_to_string(old_dir.path().join("known_hosts")).unwrap(),
        "a cleared server kept hosts the dump does not carry"
    );
    let kept: Vec<std::path::PathBuf> = std::fs::read_dir(new_dir.path())
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.to_string_lossy().contains("known_hosts.replaced-"))
        .collect();
    assert_eq!(kept.len(), 1, "{kept:?}");
    assert_eq!(
        std::fs::read_to_string(&kept[0]).unwrap(),
        format!("only.here.example.com {GITHUB_ED25519}\n")
    );
}

/// `--clear` leaves an operator's key alone, whether or not the dump
/// carries one.
#[tokio::test]
async fn clearing_leaves_an_operators_key_alone() {
    let old_dir = tempfile::tempdir().unwrap();
    let with_key = dump_with_ssh(&ssh_server(old_dir.path())).await;
    let source = aurcache_db::testing::memory_db().await;
    package(&source, "hello", true, None).await;
    let without = write_archive(&build_dump(&source, "test", None).await.unwrap()).unwrap();

    for bytes in [with_key, without] {
        let new_dir = tempfile::tempdir().unwrap();
        let other = ssh_server(&new_dir.path().join("other"));
        drop(other);
        let operator_key = new_dir.path().join("operator_key");
        std::fs::copy(new_dir.path().join("other/id_ed25519"), &operator_key).unwrap();
        let operator_bytes = std::fs::read(&operator_key).unwrap();
        let new = Arc::new(SshAccess::new(SshConfig {
            dir: new_dir.path().join("ssh"),
            key: KeySource::Provided(operator_key.clone()),
            passphrase_file: None,
            hosts: HostKeySource::TrustOnFirstUse(new_dir.path().join("ssh/known_hosts")),
        }));
        let in_use = fingerprint(&new);
        let restored = restore_with_ssh(&bytes, new, clearing()).await;
        assert!(restored.failures.is_empty(), "{:?}", restored.failures);
        assert_eq!(std::fs::read(&operator_key).unwrap(), operator_bytes);
        assert_eq!(fingerprint(restored.store.ssh()), in_use);
    }
}

/// A server that protects its key keeps a restored unprotected key
/// protected, with its own passphrase, rather than unprotected on disk.
#[tokio::test]
async fn an_unprotected_dump_key_is_protected_with_this_servers_passphrase() {
    let old_dir = tempfile::tempdir().unwrap();
    let old = ssh_server(old_dir.path());
    let bytes = dump_with_ssh(&old).await;

    let new_dir = tempfile::tempdir().unwrap();
    let key = new_dir.path().join("id_ed25519");
    if protected_key(&key, "correct horse").is_none() {
        return;
    }
    let passphrase = new_dir.path().join("passphrase");
    std::fs::write(&passphrase, "correct horse\n").unwrap();
    let new = Arc::new(SshAccess::new(SshConfig {
        dir: new_dir.path().to_path_buf(),
        key: KeySource::Generated(key.clone()),
        passphrase_file: Some(passphrase),
        hosts: HostKeySource::TrustOnFirstUse(new_dir.path().join("known_hosts")),
    }));
    let restored = restore_with_ssh(&bytes, new, copying()).await;
    assert!(restored.failures.is_empty(), "{:?}", restored.failures);
    assert_eq!(fingerprint(restored.store.ssh()), fingerprint(&old));
    assert_ne!(
        std::fs::read_to_string(&key).unwrap(),
        std::fs::read_to_string(old_dir.path().join("id_ed25519")).unwrap(),
        "the key was written unprotected"
    );
    match restored.store.ssh().info().key {
        aurcache_common::api::ssh::SshKey::Ready {
            passphrase_protected,
            ..
        } => assert!(passphrase_protected),
        other => panic!("{other:?}"),
    }
}
