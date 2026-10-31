//! The structured log end to end: emit through the queue, read it back.
//!
//! Against a real (in-memory) database rather than a mock, because what is
//! being tested is the storage: the entity index written beside each entry, and
//! the filters built on it.

use aurcache_activitylog::activity_utils::{Writer, spawn};
use aurcache_activitylog::events::{Event, RefreshTarget};
use aurcache_activitylog::log_store::{LogFilter, LogStore};
use aurcache_common::api::activity::Severity;
use aurcache_common::api::log::{BuildRef, PackageRef};
use aurcache_db::migration::Migrator;
use aurcache_db::prelude::LogEntities;
use sea_orm::{Database, DatabaseConnection, EntityTrait, PaginatorTrait};
use sea_orm_migration::MigratorTrait;

// ---------------------------------------------------------------------------

async fn db() -> DatabaseConnection {
    let db = Database::connect("sqlite::memory:").await.unwrap();
    Migrator::up(&db, None).await.unwrap();
    db
}

#[tokio::test]
async fn an_event_reaches_the_log_with_its_payload() {
    let db = db().await;
    // No package rows at all: an entry names what it names, whether or not
    // it is still there.

    let Writer { log, task: writer } = spawn(db.clone());
    log.emit_by(
        Event::VersionCompareFallback {
            pkg: "baz".into(),
            upstream_version: "2.0".to_string(),
            built_version: "1.0".to_string(),
        },
        Some("alice".to_string()),
    );
    log.emit(Event::VcsSyncFailed {
        pkg: "gone".into(),
        error: "boom".to_string(),
    });
    drop(log);
    writer.await.unwrap();

    let page = LogStore::new(db)
        .page(50, 0, &LogFilter::default())
        .await
        .unwrap();
    assert_eq!(page.total, 2);

    let compared = page
        .entries
        .iter()
        .find(|e| e.kind == "version.compare_fallback")
        .expect("the comparison entry");
    assert_eq!(compared.severity, Severity::Warning);
    assert_eq!(compared.user.as_deref(), Some("alice"));
    assert_eq!(compared.data["pkg"], "pkg:baz");
    assert_eq!(compared.data["upstream_version"], "2.0");
    assert!(
        compared.message.contains("cannot compare"),
        "{}",
        compared.message
    );

    let vanished = page
        .entries
        .iter()
        .find(|e| e.kind == "vcs.sync_failed")
        .expect("the vcs entry");
    // It reads without the package, because the sentence was rendered when it
    // was written -- naming it as people do, without the namespace.
    assert!(
        vanished.message.contains("sources of gone:"),
        "{}",
        vanished.message
    );
}

/// The entity filter finds a row by what it names, and the query names no
/// kind -- so it works for kinds that did not exist when it was written.
///
/// Every event in the catalogue today names one entity, so the "whatever role
/// it played" half is covered by the role-scoped case below and by
/// `event::tests`; it gets its full demonstration once a multi-reference event
/// (a dependency replacement) is ported.
#[tokio::test]
async fn the_entity_filter_finds_a_row_in_any_role() {
    let db = db().await;
    let Writer { log, task: writer } = spawn(db.clone());
    log.emit(Event::VcsSyncFailed {
        pkg: "baz".into(),
        error: "boom".to_string(),
    });
    log.emit(Event::VcsSyncFailed {
        pkg: "unrelated".into(),
        error: "boom".to_string(),
    });
    drop(log);
    writer.await.unwrap();

    let store = LogStore::new(db);
    for role_played in ["baz", "unrelated"] {
        let page = store
            .page(
                50,
                0,
                &LogFilter {
                    entity: Some(PackageRef::from(role_played).into()),
                    ..LogFilter::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(page.total, 1, "{role_played}");
    }

    // Narrowed to the role it actually played, it matches; narrowed to another,
    // it does not.
    let as_pkg = store
        .page(
            50,
            0,
            &LogFilter {
                entity: Some(PackageRef::from("baz").into()),
                role: Some("pkg".to_string()),
                ..LogFilter::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(as_pkg.total, 1);

    let as_dependent = store
        .page(
            50,
            0,
            &LogFilter {
                entity: Some(PackageRef::from("baz").into()),
                role: Some("dependent".to_string()),
                ..LogFilter::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(as_dependent.total, 0);
}

/// Severity is stored as an ordered number, so the filter is "this and worse"
/// without knowing which kinds exist.
#[tokio::test]
async fn severity_narrows_to_this_and_worse() {
    let db = db().await;
    let Writer { log, task: writer } = spawn(db.clone());
    log.emit(Event::AurMissing { pkg: "baz".into() });
    log.emit(Event::VcsSyncFailed {
        pkg: "hello".into(),
        error: "boom".to_string(),
    });
    log.emit(Event::UpdateQueueFailed {
        error: "no space left on device".to_string(),
    });
    drop(log);
    writer.await.unwrap();

    let store = LogStore::new(db);
    let all = store.page(50, 0, &LogFilter::default()).await.unwrap();
    assert_eq!(all.total, 3);

    let warnings = store
        .page(
            50,
            0,
            &LogFilter {
                severity: Some(Severity::Warning),
                ..LogFilter::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        warnings.total, 3,
        "every catalogue event today is a warning or worse"
    );
    assert!(
        warnings
            .entries
            .iter()
            .all(|e| e.severity >= Severity::Warning)
    );

    let errors = store
        .page(
            50,
            0,
            &LogFilter {
                severity: Some(Severity::Error),
                ..LogFilter::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(errors.total, 1);
    assert_eq!(errors.entries[0].kind, "update.queue_failed");
}

/// A scoped handle files everything it records under one entity, so a build's
/// page can ask for both what was said about it and what happened during it.
#[tokio::test]
async fn a_scope_is_indexed_like_any_other_reference() {
    let db = db().await;
    let build = BuildRef {
        pkgbase: "hello".into(),
        number: 7,
    };

    let Writer { log, task: writer } = spawn(db.clone());
    // Emitted *during* the build: the payload names it nowhere.
    log.scoped(build.clone())
        .emit(Event::AurMissing { pkg: "baz".into() });
    drop(log);
    writer.await.unwrap();

    let page = LogStore::new(db)
        .page(
            50,
            0,
            &LogFilter {
                entity: Some(build.clone().into()),
                ..LogFilter::default()
            },
        )
        .await
        .unwrap();

    assert_eq!(page.total, 1);
    assert_eq!(page.entries[0].scope, Some(build.into()));
}

/// With no start entry to count back to, "since the last restart" shows
/// everything rather than nothing.
#[tokio::test]
async fn since_boot_without_a_marker_shows_everything() {
    let db = db().await;
    let Writer { log, task: writer } = spawn(db.clone());
    log.emit(Event::AurMissing {
        pkg: "hello".into(),
    });
    drop(log);
    writer.await.unwrap();

    let page = LogStore::new(db)
        .page(
            50,
            0,
            &LogFilter {
                since_boot: true,
                ..LogFilter::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(page.total, 1, "no marker means no narrowing");
}

/// One kind covering several cases keeps the difference as a field, so the
/// consolidation loses nothing: the case is in the payload and in the sentence.
#[tokio::test]
async fn a_consolidated_kind_keeps_which_case_it_was() {
    let db = db().await;
    let Writer { log, task: writer } = spawn(db.clone());
    log.emit(Event::SourceRefreshFailed {
        pkg: "hello".into(),
        target: RefreshTarget::Git,
        error: "host is unreachable".to_string(),
    });
    log.emit(Event::SourceRefreshFailed {
        pkg: "yay".into(),
        target: RefreshTarget::Snapshot,
        error: "checksum mismatch".to_string(),
    });
    drop(log);
    writer.await.unwrap();

    let both = LogStore::new(db)
        .page(
            50,
            0,
            &LogFilter {
                kind: Some("source.refresh_failed".to_string()),
                ..LogFilter::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(both.total, 2, "one kind covers both");

    let mut cases: Vec<_> = both
        .entries
        .iter()
        .map(|e| e.data["target"].as_str().unwrap().to_string())
        .collect();
    cases.sort();
    assert_eq!(cases, ["git", "snapshot"]);

    // And the sentence says which, so a reader needs neither the payload nor a
    // catalogue.
    assert!(
        both.entries
            .iter()
            .any(|e| e.message.contains("git source")),
        "{:?}",
        both.entries
    );
}

/// Pruning takes the index with it, or the filter would keep finding entries
/// that are gone.
#[tokio::test]
async fn pruning_removes_the_index_too() {
    let db = db().await;
    let Writer { log, task: writer } = spawn(db.clone());
    log.emit(Event::AurMissing { pkg: "baz".into() });
    drop(log);
    writer.await.unwrap();

    let store = LogStore::new(db.clone());
    let now = aurcache_db::helpers::time::now_secs();
    assert_eq!(store.prune(3600, now + 7200).await.unwrap(), 1);

    let orphans = LogEntities::find().count(&db).await.unwrap();
    assert_eq!(orphans, 0, "the cascade should have taken the index rows");
}

/// A row written straight to the table, at a chosen time: the queue stamps
/// entries with the clock, and these tests are about when things happened.
async fn at(db: &DatabaseConnection, timestamp: i64, event: Event) {
    use sea_orm::{ActiveModelTrait, ActiveValue::Set};
    let wire = serde_json::to_value(&event).unwrap();
    aurcache_db::logs::ActiveModel {
        kind: Set(event.kind().to_string()),
        severity: Set(event.severity()),
        message: Set(event.message()),
        data: Set(wire["data"].to_string()),
        timestamp: Set(timestamp),
        ..Default::default()
    }
    .insert(db)
    .await
    .unwrap();
}

/// "Since the last restart" counts back to the newest start entry, so what
/// happened under an earlier run drops out.
#[tokio::test]
async fn since_boot_cuts_at_the_last_start() {
    let db = db().await;
    let started = |version: &str| Event::ServerStarted {
        version: version.to_string(),
    };
    at(&db, 100, started("1.0")).await;
    at(&db, 150, Event::AurMissing { pkg: "old".into() }).await;
    at(&db, 200, started("1.1")).await;
    at(&db, 250, Event::AurMissing { pkg: "new".into() }).await;

    let page = LogStore::new(db)
        .page(
            50,
            0,
            &LogFilter {
                since_boot: true,
                ..LogFilter::default()
            },
        )
        .await
        .unwrap();
    let messages: Vec<_> = page.entries.iter().map(|e| e.message.as_str()).collect();
    assert_eq!(
        messages,
        ["new is no longer in the AUR", "AURCache 1.1 started"]
    );
}

/// Pruning takes what has aged out and leaves the rest; zero keeps
/// everything, for a deployment that would rather the log were complete.
#[tokio::test]
async fn pruning_keeps_what_is_still_within_the_window() {
    let db = db().await;
    let now = aurcache_db::helpers::time::now_secs();
    at(&db, now - 7200, Event::AurMissing { pkg: "old".into() }).await;
    at(&db, now, Event::AurMissing { pkg: "new".into() }).await;

    let store = LogStore::new(db);
    assert_eq!(store.prune(0, now).await.unwrap(), 0);
    assert_eq!(store.prune(3600, now).await.unwrap(), 1);
    let left = store.page(50, 0, &LogFilter::default()).await.unwrap();
    assert_eq!(left.total, 1);
    assert!(left.entries[0].message.starts_with("new"));
}

/// Paging has to be stable across entries written in the same second, which
/// is the ordinary case for a bulk add.
#[tokio::test]
async fn pages_do_not_drop_or_repeat_an_entry() {
    let db = db().await;
    for n in 0..5 {
        at(
            &db,
            100,
            Event::AurMissing {
                pkg: format!("p{n}").into(),
            },
        )
        .await;
    }
    let store = LogStore::new(db);
    let mut seen = Vec::new();
    for offset in [0, 2, 4] {
        let page = store.page(2, offset, &LogFilter::default()).await.unwrap();
        assert_eq!(page.total, 5);
        seen.extend(page.entries.into_iter().map(|e| e.id));
    }
    seen.sort_unstable();
    seen.dedup();
    assert_eq!(seen.len(), 5);
}

/// Everything about a package includes its builds: an entry naming only a
/// build of it -- or recorded during one -- is still about the package.
#[tokio::test]
async fn a_package_filter_finds_entries_about_its_builds() {
    let db = db().await;
    let Writer { log, task: writer } = spawn(db.clone());
    let build = BuildRef {
        pkgbase: "yay".into(),
        number: 7,
    };
    log.emit(Event::PublishFailed {
        build: build.clone(),
        error: "disk full".to_string(),
    });
    log.scoped(build)
        .emit(Event::AurMissing { pkg: "dep".into() });
    log.emit(Event::PackageAdded {
        pkg: "hello".into(),
    });
    drop(log);
    writer.await.unwrap();

    let page = LogStore::new(db)
        .page(
            50,
            0,
            &LogFilter {
                entity: Some(PackageRef::from("yay").into()),
                ..LogFilter::default()
            },
        )
        .await
        .unwrap();
    let mut kinds: Vec<_> = page.entries.iter().map(|e| e.kind.as_str()).collect();
    kinds.sort_unstable();
    assert_eq!(kinds, ["publish.failed", "version_check.aur_missing"]);
}
