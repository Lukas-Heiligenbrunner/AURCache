//! Upgrading a 0.5.0 database: the migration runs after upstream's own, on
//! rows shaped the way 0.5.0 wrote them.

use crate::migration::Migrator;
use sea_orm::{ConnectionTrait, Database, DatabaseConnection, Statement};
use sea_orm_migration::MigratorTrait;

/// Upstream 0.5.0's own migrations, all of which come before this one.
const UPSTREAM: u32 = 7;

/// A 0.5.0 instance with something for every conversion: a package added by
/// a split package's name twice, one added twice under its own, a build for
/// no platform, builds without a status, logs in the
/// database, two pending builds for one platform, one it was running when it
/// stopped, files linked through the old table, an interval schedule, a retired setting, and rows whose package
/// is gone.
const SEED: &str = r#"
INSERT INTO packages (id, name, status, out_of_date, upstream_version, latest_build, build_flags, platforms, source_type, source_data) VALUES
 (1, 'hello', 1, 1, '1.1-1', 3, '-Byu;--noconfirm;--noprogressbar;--color never', 'x86_64;aarch64', 'aur', '{"type":"aur","name":"hello"}'),
 (2, 'czkawka-cli', 1, 0, '2.0-1', 5, '--pgpfetch;-B;--noconfirm;--noprogressbar;--color never', 'x86_64', 'aur', '{"type":"aur","name":"czkawka"}'),
 (3, 'czkawka-gui', 2, 0, '2.0-1', 6, '--pgpfetch;-B;--noconfirm;--noprogressbar;--color never;--skipreview', 'x86_64', 'aur', '{"type":"aur","name":"czkawka"}'),
 (4, 'hello', 1, 0, '1.1-1', NULL, '', 'x86_64', 'aur', '{"type":"aur","name":"hello"}');
INSERT INTO builds (id, pkg_id, output, status, start_time, end_time, platform, version) VALUES
 (1, 1, 'first log', 2, 100, 110, 'x86_64', '1.0-1'),
 (2, 1, '', 1, 200, 210, 'x86_64', '1.0-1'),
 (3, 1, 'third log', 1, 300, 310, 'x86_64', '1.1-1'),
 (4, 1, NULL, 3, 400, NULL, 'aarch64', '1.1-1'),
 (5, 2, 'cli log', 1, 500, 510, 'x86_64', '2.0-1'),
 (6, 3, 'gui log', 2, 600, 610, 'x86_64', '2.0-1'),
 (7, 1, NULL, 3, 700, NULL, 'aarch64', '1.1-1'),
 (8, 1, NULL, NULL, NULL, 800, 'x86_64', '1.1-1'),
 (9, 3, 'partial log', 0, 900, NULL, 'x86_64', '2.0-1'),
 (10, 99, 'orphan', 1, 1000, 1010, 'x86_64', '0.1-1'),
 (11, 1, 'no platform', 2, 1100, 1110, '', '1.1-1');
INSERT INTO files (id, filename, platform) VALUES
 (1, 'hello-1.1-1-x86_64.pkg.tar.zst', 'x86_64'),
 (2, 'czkawka-cli-2.0-1-x86_64.pkg.tar.zst', 'x86_64'),
 (3, 'stray-1.0-1-x86_64.pkg.tar.zst', 'x86_64');
INSERT INTO packages_files (id, file_id, package_id) VALUES (1, 1, 1), (2, 2, 2);
INSERT INTO settings (id, key, value, pkg_id) VALUES
 (1, 'auto_update_interval', '0 0 2 * * 1', -1),
 (2, 'cpu_limit', '2', -1),
 (3, 'job_timeout', '60', 2);
"#;

/// Where the upgrades write build logs: one directory for every test, set
/// once, because the variable it is read from is the whole process's.
fn logs() -> &'static std::path::Path {
    static LOGS: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
    LOGS.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap();
        // SAFETY: set once, before any upgrade here reads it.
        unsafe { std::env::set_var("AURCACHE_BUILD_LOG_PATH", dir.path()) };
        dir
    })
    .path()
}

/// A seeded 0.5.0 database, upgraded.
async fn upgraded() -> DatabaseConnection {
    let db = Database::connect("sqlite::memory:").await.unwrap();
    Migrator::up(&db, Some(UPSTREAM)).await.unwrap();
    db.execute_unprepared(SEED).await.unwrap();
    logs();
    Migrator::up(&db, None).await.unwrap();
    db
}

/// The first column of every row `sql` returns, as text, in order.
async fn column(db: &DatabaseConnection, sql: &str) -> Vec<String> {
    db.query_all_raw(Statement::from_string(db.get_database_backend(), sql))
        .await
        .unwrap()
        .into_iter()
        .map(|row| row.try_get_by_index::<String>(0).unwrap())
        .collect()
}

#[tokio::test]
async fn a_0_5_database_is_upgraded() {
    let db = upgraded().await;

    // One row per package base, keeping the one built most recently; 0.5.0's
    // two paru defaults (the older one as its own migration left it) are now
    // the makepkg default, and what makepkg would reject is gone.
    assert_eq!(
        column(
            &db,
            "SELECT id || ' ' || name || ' ' || build_flags FROM packages ORDER BY id"
        )
        .await,
        [
            "1 hello --noconfirm;--noprogressbar;--nocolor",
            "3 czkawka --noconfirm;--noprogressbar;--nocolor",
        ]
    );
    // Numbered per package in the order they were made; the duplicate
    // pending build and the one without a status failed; the one still
    // pending waits for the backfill; the orphan gone; a missing start time
    // filled from the end.
    assert_eq!(
        column(
            &db,
            "SELECT id || ' #' || number || ' status=' || status || ' start=' || start_time \
             FROM builds WHERE id < 11 ORDER BY id"
        )
        .await,
        [
            "1 #1 status=2 start=100",
            "2 #2 status=1 start=200",
            "3 #3 status=1 start=300",
            "4 #4 status=2 start=400",
            "6 #1 status=2 start=600",
            "7 #5 status=4 start=700",
            "8 #6 status=2 start=800",
            "9 #2 status=2 start=900",
        ]
    );
    // The build 0.5.0 was running died with it: interrupted, with a fresh one
    // waiting in its place.
    assert_eq!(
        column(
            &db,
            "SELECT id || ' end_reason=' || end_reason || ' ended=' || (end_time > 0) \
             FROM builds WHERE id = 9"
        )
        .await,
        ["9 end_reason=5 ended=1"]
    );
    assert_eq!(
        column(
            &db,
            "SELECT pkg_id || ' #' || number || ' ' || platform || ' ' || version \
             || ' status=' || status || ' trigger=' || \"trigger\" FROM builds WHERE \"trigger\" = 2"
        )
        .await,
        ["3 #3 x86_64 2.0-1 status=4 trigger=2"]
    );
    // Derived from the newest build per platform: one is enqueued.
    assert_eq!(
        column(
            &db,
            "SELECT name || ' ' || status FROM packages ORDER BY id"
        )
        .await,
        ["hello 4", "czkawka 4"]
    );
    // Logs left the database, named after the build; an empty one made no
    // file.
    let read = |path: &str| std::fs::read_to_string(logs().join(path)).ok();
    assert_eq!(read("hello/1.log").as_deref(), Some("first log"));
    assert_eq!(read("hello/2.log"), None);
    assert_eq!(read("hello/3.log").as_deref(), Some("third log"));
    assert_eq!(read("czkawka/1.log").as_deref(), Some("gui log"));
    assert_eq!(read("czkawka/2.log").as_deref(), Some("partial log"));
    // Each file belongs to its package, one whose package was merged away to
    // the package it was merged into; one with none is gone.
    assert_eq!(
        column(
            &db,
            "SELECT filename || ' ' || package_id FROM files ORDER BY id"
        )
        .await,
        [
            "hello-1.1-1-x86_64.pkg.tar.zst 1",
            "czkawka-cli-2.0-1-x86_64.pkg.tar.zst 3"
        ]
    );
    // Recording their dependencies is queued for the server, over both.
    assert_eq!(
        column(
            &db,
            "SELECT kind || ' ' || total FROM operations WHERE finished_at IS NULL"
        )
        .await,
        ["dependency_backfill 2"]
    );
    // The seconds-first schedule became a crontab one, Sunday 1 now 0, under
    // its new key; the server-wide build limit stays for the embedded worker
    // to take over, and the merged package's setting is gone.
    assert_eq!(
        column(
            &db,
            "SELECT key || '=' || value || ' @' || pkg_id FROM settings ORDER BY id"
        )
        .await,
        ["auto_update_schedule=0 2 * * 0 @-1", "cpu_limit=2 @-1"]
    );
}

#[tokio::test]
async fn the_new_constraints_hold() {
    let db = upgraded().await;
    let fails = |sql: &'static str| {
        let db = db.clone();
        async move { db.execute_unprepared(sql).await.is_err() }
    };

    assert!(
        fails("INSERT INTO builds (pkg_id, number, platform, status) VALUES (1, 9, 'aarch64', 3)")
            .await,
        "a second pending build for one package and platform"
    );
    assert!(
        fails("INSERT INTO builds (pkg_id, number, platform, status) VALUES (42, 1, 'x86_64', 1)")
            .await,
        "a build of no package"
    );
    assert!(
        fails("INSERT INTO builds (pkg_id, number, platform) VALUES (1, 9, 'x86_64')").await,
        "a build without a status"
    );
    db.execute_unprepared("INSERT INTO dependencies (dependent_id, dependee_id) VALUES (1, 3)")
        .await
        .unwrap();
    assert!(
        fails("INSERT INTO dependencies (dependent_id, dependee_id) VALUES (1, 3)").await,
        "a second edge between the same packages"
    );
    // One file name per platform: an `arch=any` package built for two
    // platforms has the same name in each.
    db.execute_unprepared(
        "INSERT INTO files (filename, platform, package_id) VALUES \
         ('hello-1.1-1-any.pkg.tar.zst', 'x86_64', 1), ('hello-1.1-1-any.pkg.tar.zst', 'aarch64', 1)",
    )
    .await
    .unwrap();
    assert!(
        fails(
            "INSERT INTO files (filename, platform, package_id) \
             VALUES ('hello-1.1-1-any.pkg.tar.zst', 'x86_64', 1)"
        )
        .await,
        "the same file name twice on one platform"
    );
    db.execute_unprepared("INSERT INTO api_tokens (username, token_hash) VALUES ('a', 'h')")
        .await
        .unwrap();
    assert!(
        fails("INSERT INTO api_tokens (username, token_hash) VALUES ('b', 'h')").await,
        "two users with one token"
    );

    // A package takes its builds, files and edges with it.
    db.execute_unprepared("DELETE FROM packages WHERE id = 1")
        .await
        .unwrap();
    for table in [
        "builds WHERE pkg_id = 1",
        "files WHERE package_id = 1",
        "dependencies",
    ] {
        assert_eq!(
            column(&db, &format!("SELECT CAST(COUNT(*) AS TEXT) FROM {table}")).await,
            ["0"],
            "{table}"
        );
    }
}

/// The name is what `seaql_migrations` records, so it must not drift.
#[test]
fn the_migration_is_recorded_under_its_own_name() {
    use sea_orm_migration::MigrationName;
    assert_eq!(super::Migration.name(), "m20261008_000000_from_0_5");
}

/// There is no way back to 0.5.0's schema, and the migration says so rather
/// than leaving a half-reverted database.
#[tokio::test]
async fn going_back_to_0_5_is_refused() {
    let db = crate::testing::memory_db().await;
    assert!(Migrator::down(&db, None).await.is_err());
}

/// A new instance has nothing to backfill, so nothing is queued.
#[tokio::test]
async fn a_new_instance_queues_no_backfill() {
    let db = crate::testing::memory_db().await;
    assert_eq!(
        column(&db, "SELECT CAST(COUNT(*) AS TEXT) FROM operations").await,
        ["0"]
    );
}

/// 0.5.0 stored a package under the name it was added by, which for a split
/// package is not its base; the repository database records the base.
#[tokio::test]
async fn split_packages_take_their_base_from_the_repository() {
    let repo = tempfile::tempdir().unwrap();
    let dir = repo.path().join("x86_64");
    std::fs::create_dir(&dir).unwrap();
    let file = std::fs::File::create(dir.join("repo.db.tar.gz")).unwrap();
    let mut archive = tar::Builder::new(flate2::write::GzEncoder::new(
        file,
        flate2::Compression::default(),
    ));
    for name in ["nvidia-470xx-utils", "nvidia-470xx-dkms"] {
        let desc = format!(
            "%FILENAME%\n{name}-470-1-x86_64.pkg.tar.zst\n\n%NAME%\n{name}\n\n%BASE%\nnvidia-470xx\n\n"
        );
        let mut header = tar::Header::new_gnu();
        header.set_size(desc.len() as u64);
        header.set_mode(0o644);
        archive
            .append_data(&mut header, format!("{name}-470-1/desc"), desc.as_bytes())
            .unwrap();
    }
    archive.into_inner().unwrap().finish().unwrap();
    let bases = super::packages::bases_in_repo(repo.path());

    let db = Database::connect("sqlite::memory:").await.unwrap();
    Migrator::up(&db, Some(UPSTREAM)).await.unwrap();
    db.execute_unprepared(
        r#"
INSERT INTO packages (id, name, status, out_of_date, upstream_version, latest_build, build_flags, platforms, source_type, source_data) VALUES
 (1, 'nvidia-470xx-utils', 1, 0, '470-1', 2, '', 'x86_64', 'aur', '{"type":"aur","name":"nvidia-470xx-utils"}'),
 (2, 'nvidia-470xx-dkms', 1, 0, '470-1', 1, '', 'x86_64', 'aur', '{"type":"aur","name":"nvidia-470xx-dkms"}'),
 (3, 'yay', 1, 0, '12-1', NULL, '', 'x86_64', 'aur', '{"type":"aur","name":"yay"}');
INSERT INTO files (id, filename, platform) VALUES
 (1, 'nvidia-470xx-utils-470-1-x86_64.pkg.tar.zst', 'x86_64'),
 (2, 'nvidia-470xx-dkms-470-1-x86_64.pkg.tar.zst', 'x86_64');
INSERT INTO packages_files (id, file_id, package_id) VALUES (1, 1, 1), (2, 2, 1), (3, 1, 2), (4, 2, 2);
"#,
    )
    .await
    .unwrap();
    let manager = sea_orm_migration::SchemaManager::new(&db);
    super::schema::create_tables(&manager).await.unwrap();
    super::schema::add_columns(&manager).await.unwrap();
    super::packages::merge_file_links(&manager).await.unwrap();
    super::packages::normalize_package_names_and_merge_duplicates(&db, &bases)
        .await
        .unwrap();

    // One package, named after the base and built from it; the other name's
    // file is its now. A package without a recorded base keeps its name.
    assert_eq!(
        column(
            &db,
            "SELECT id || ' ' || name || ' ' || source_data FROM packages ORDER BY id"
        )
        .await,
        [
            r#"1 nvidia-470xx {"type":"aur","name":"nvidia-470xx"}"#,
            r#"3 yay {"type":"aur","name":"yay"}"#
        ]
    );
    assert_eq!(
        column(
            &db,
            "SELECT filename || ' ' || package_id FROM files ORDER BY id"
        )
        .await,
        [
            "nvidia-470xx-utils-470-1-x86_64.pkg.tar.zst 1",
            "nvidia-470xx-dkms-470-1-x86_64.pkg.tar.zst 1"
        ]
    );
}
