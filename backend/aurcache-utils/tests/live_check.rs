//! What a removal collects, and what it leaves alone.

use aurcache_common::build_state::BuildState;
use aurcache_db::migration::Migrator;
use aurcache_db::packages::{self, SourceData};
use aurcache_db::{dependencies, prelude::Dependencies};
use aurcache_utils::package::delete::package_delete;
use aurcache_utils::package::live_check::package_remove;
use aurcache_utils::repository::Repository;
use aurcache_utils::snapshot::SnapshotStore;
use sea_orm::{ActiveModelTrait, Database, DatabaseConnection, EntityTrait, PaginatorTrait, Set};
use sea_orm_migration::MigratorTrait;

/// A store over a checkout root that holds nothing: these tests are about which
/// rows go, and removing a checkout that was never made is a no-op.
fn store() -> SnapshotStore {
    SnapshotStore::with_checkout_root(std::env::temp_dir().join("aurcache-live-check-no-checkouts"))
}

/// A repository nothing was ever published to.
fn repo() -> Repository {
    Repository::new(tempfile::tempdir().unwrap().keep())
}

async fn memory_db() -> DatabaseConnection {
    let db = Database::connect("sqlite::memory:").await.unwrap();
    Migrator::up(&db, None).await.unwrap();
    db
}

async fn package(db: &DatabaseConnection, name: &str, directly_requested: bool) -> i32 {
    packages::ActiveModel {
        name: Set(name.to_string()),
        status: Set(BuildState::Enqueued),
        out_of_date: Set(false),
        upstream_version: Set(None),
        build_flags: Set(Default::default()),
        platforms: Set("x86_64".parse().unwrap()),
        source_data: Set(SourceData::Aur { name: name.into() }),
        directly_requested: Set(directly_requested),
        split_packages: Set(None),
        ..Default::default()
    }
    .save(db)
    .await
    .unwrap()
    .id
    .unwrap()
}

async fn needs(db: &DatabaseConnection, dependent: i32, dependee: i32) {
    dependencies::ActiveModel {
        dependent_id: Set(dependent),
        dependee_id: Set(dependee),
        version_constraint: Set(String::new()),
        ..Default::default()
    }
    .save(db)
    .await
    .unwrap();
}

async fn remaining(db: &DatabaseConnection) -> Vec<String> {
    let mut names: Vec<String> = packages::Entity::find()
        .all(db)
        .await
        .unwrap()
        .into_iter()
        .map(|package| package.name)
        .collect();
    names.sort();
    names
}

/// The ordinary case: removing the requested package takes the chain of
/// dependencies below it, and the links along with them.
#[tokio::test]
async fn removing_a_root_collects_the_chain_under_it() {
    let db = memory_db().await;
    let root = package(&db, "root", true).await;
    let middle = package(&db, "middle", false).await;
    let leaf = package(&db, "leaf", false).await;
    needs(&db, root, middle).await;
    needs(&db, middle, leaf).await;

    package_remove(&db, &store(), &repo(), root).await.unwrap();

    assert!(remaining(&db).await.is_empty());
    assert_eq!(Dependencies::find().count(&db).await.unwrap(), 0);
}

/// A dependency two packages share outlives the first of them.
#[tokio::test]
async fn a_shared_dependency_waits_for_its_last_dependent() {
    let db = memory_db().await;
    let first = package(&db, "first", true).await;
    let second = package(&db, "second", true).await;
    let shared = package(&db, "shared", false).await;
    needs(&db, first, shared).await;
    needs(&db, second, shared).await;

    package_remove(&db, &store(), &repo(), first).await.unwrap();
    assert_eq!(remaining(&db).await, ["second", "shared"]);

    package_remove(&db, &store(), &repo(), second)
        .await
        .unwrap();
    assert!(remaining(&db).await.is_empty());
}

/// Two dependencies that need each other keep each other alive under a
/// per-package "does anything depend on me" rule, and used to be left behind
/// with nothing above them. Rare in the AUR, but permanent when it happened.
#[tokio::test]
async fn a_dependency_cycle_is_collected_with_its_root() {
    let db = memory_db().await;
    let root = package(&db, "root", true).await;
    let left = package(&db, "left", false).await;
    let right = package(&db, "right", false).await;
    needs(&db, root, left).await;
    needs(&db, left, right).await;
    needs(&db, right, left).await;

    package_remove(&db, &store(), &repo(), root).await.unwrap();

    assert!(remaining(&db).await.is_empty());
}

/// A cycle that a requested package still needs stays put: reachability, not
/// "is it in a cycle", is what decides.
#[tokio::test]
async fn a_cycle_something_still_needs_is_kept() {
    let db = memory_db().await;
    let leaving = package(&db, "leaving", true).await;
    let staying = package(&db, "staying", true).await;
    let left = package(&db, "left", false).await;
    let right = package(&db, "right", false).await;
    needs(&db, leaving, left).await;
    needs(&db, staying, left).await;
    needs(&db, left, right).await;
    needs(&db, right, left).await;

    package_remove(&db, &store(), &repo(), leaving)
        .await
        .unwrap();

    assert_eq!(remaining(&db).await, ["left", "right", "staying"]);
}

/// Nothing outside what the removed package reached is touched, even when it
/// is unreferenced itself — an add that has inserted a package but not yet
/// linked it up is not this removal's to collect.
#[tokio::test]
async fn an_unrelated_unlinked_package_is_left_alone() {
    let db = memory_db().await;
    let root = package(&db, "root", true).await;
    let dependency = package(&db, "dependency", false).await;
    needs(&db, root, dependency).await;
    package(&db, "in-flight", false).await;

    package_remove(&db, &store(), &repo(), root).await.unwrap();

    assert_eq!(remaining(&db).await, ["in-flight"]);
}

/// The directory the store keeps an AUR source's checkout in.
fn checkout_of(root: &std::path::Path, name: &str) -> std::path::PathBuf {
    root.join(format!("aur_{name}"))
}

/// A removed package's checkout goes with its rows.
#[tokio::test]
async fn a_collected_package_takes_its_checkout() {
    let db = memory_db().await;
    let checkouts = tempfile::tempdir().unwrap();
    let store = SnapshotStore::with_checkout_root(checkouts.path().to_path_buf());
    let root = package(&db, "root", true).await;
    let dependency = package(&db, "dependency", false).await;
    needs(&db, root, dependency).await;
    for name in ["root", "dependency"] {
        std::fs::create_dir_all(checkout_of(checkouts.path(), name)).unwrap();
    }

    package_remove(&db, &store, &repo(), root).await.unwrap();

    assert!(remaining(&db).await.is_empty());
    for name in ["root", "dependency"] {
        assert!(
            !checkout_of(checkouts.path(), name).exists(),
            "{name}'s checkout outlived it"
        );
    }
}

/// Removing a package something still depends on only stops it being
/// requested: its row, and its checkout, stay.
#[tokio::test]
async fn a_package_still_depended_on_keeps_its_checkout() {
    let db = memory_db().await;
    let checkouts = tempfile::tempdir().unwrap();
    let store = SnapshotStore::with_checkout_root(checkouts.path().to_path_buf());
    let dependent = package(&db, "dependent", true).await;
    let removed = package(&db, "removed", true).await;
    needs(&db, dependent, removed).await;
    std::fs::create_dir_all(checkout_of(checkouts.path(), "removed")).unwrap();

    package_remove(&db, &store, &repo(), removed).await.unwrap();

    assert_eq!(remaining(&db).await, ["dependent", "removed"]);
    assert!(checkout_of(checkouts.path(), "removed").is_dir());
}

/// The delete itself refuses a package something outside the batch needs, so
/// no caller can take rows, artifacts or checkout out from under a dependent.
#[tokio::test]
async fn deleting_a_package_something_needs_is_refused() {
    let db = memory_db().await;
    let dependent = package(&db, "dependent", true).await;
    let needed = package(&db, "needed", false).await;
    needs(&db, dependent, needed).await;

    let refused = package_delete(&db, &store(), &repo(), &[needed]).await;

    assert!(refused.is_err(), "deleted a package that is still needed");
    assert_eq!(remaining(&db).await, ["dependent", "needed"]);
    assert_eq!(Dependencies::find().count(&db).await.unwrap(), 1);

    // Together with what needs it, it can go.
    package_delete(&db, &store(), &repo(), &[dependent, needed])
        .await
        .unwrap();
    assert!(remaining(&db).await.is_empty());
}

/// What an update re-synced away: its former dependencies are the starting
/// points, and a cycle among them goes -- which the whole-table sweep this
/// replaced could never collect, since every package in a cycle has a
/// dependent. The package being updated is held, even when the cycle leads
/// back to it.
#[tokio::test]
async fn an_update_collects_the_cycle_it_dropped_but_never_itself() {
    let db = memory_db().await;
    // `mid` is a dependency-only package being updated, its own dependent cut
    // off; `a` and `b` need each other and `b` needs `mid` back.
    let mid = package(&db, "mid", false).await;
    let a = package(&db, "a", false).await;
    let b = package(&db, "b", false).await;
    needs(&db, a, b).await;
    needs(&db, b, a).await;
    needs(&db, b, mid).await;

    aurcache_utils::package::live_check::live_check(&db, &store(), &repo(), &[a], &[mid])
        .await
        .unwrap();

    assert_eq!(remaining(&db).await, ["mid"]);
}
