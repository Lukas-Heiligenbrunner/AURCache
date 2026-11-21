//! The server's worker listener, run in-process over an in-memory database
//! and a throwaway CA, for a worker to enroll with through the production
//! client.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aurcache_utils::repository::Repository;
use aurcache_worker_core::client::fetch_and_pin_ca;
use sea_orm::{ConnectionTrait, DatabaseConnection};

/// How long a wait goes on before it is taken for a hang. Not how long
/// anything should take: publishing syncs the repository to disk, and on a
/// busy machine that alone can take tens of seconds.
pub const STUCK: Duration = Duration::from_secs(180);

/// The enrollment token, which makes the server approve the worker at once.
const TOKEN: &str = "s3cr3t-enroll-token";

/// An enqueued build of package `p<id>`. Its source is a git path that does
/// not exist, so the server's `.SRCINFO` fetch on claim fails at once
/// instead of reaching the network.
pub async fn seed_build(db: &DatabaseConnection, id: i32, platform: &str, start: i64) {
    let source =
        r#"{"type":"git","url":"/nonexistent-aurcache-fake-worker","ref":"HEAD","subfolder":""}"#;
    db.execute_unprepared(&format!(
        "INSERT INTO packages (id, name, build_flags, source_data, platforms) \
         VALUES ({id}, 'p{id}', '', '{source}', '{platform}')"
    ))
    .await
    .unwrap();
    db.execute_unprepared(&format!(
        "INSERT INTO builds (id, pkg_id, status, start_time, platform, version) \
         VALUES ({id}, {id}, 3, {start}, '{platform}', '1.0')"
    ))
    .await
    .unwrap();
}

/// Start the listener over `db` and `repo` on a free port, and point this
/// process's worker configuration at it, with its data under `dir/worker`.
/// Both are read from the environment, so a test file calls this once.
pub async fn start_listener(
    dir: &Path,
    db: &DatabaseConnection,
    repo: Arc<Repository>,
    worker_name: &str,
) {
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    // SAFETY: before any worker or server thread reads these; each
    // `tests/*.rs` file is its own process.
    unsafe {
        std::env::set_var("AURCACHE_WORKER_PORT", port.to_string());
        std::env::set_var("AURCACHE_TLS_SANS", "localhost");
        std::env::set_var("AURCACHE_ENROLLMENT_TOKEN", TOKEN);
        std::env::remove_var("AURCACHE_ENROLLMENT_DIR");
        std::env::remove_var("AURCACHE_PREAPPROVED_WORKERS");
        std::env::set_var("AURCACHE_URL", format!("https://localhost:{port}"));
        std::env::set_var("WORKER_DATA_DIR", dir.join("worker"));
        std::env::set_var("WORKER_ARCHES", "x86_64");
        std::env::set_var("WORKER_NAME", worker_name);
        std::env::remove_var("WORKER_CONCURRENCY");
    }

    let ca = aurcache_ca::Ca::load_or_create(&dir.join("ca")).unwrap();
    // Runs for as long as the test's runtime does.
    drop(aurcache_api::init::init_worker_api(
        db.clone(),
        ca,
        Arc::new(aurcache_utils::snapshot::SnapshotStore::new()),
        repo,
        aurcache_activitylog::activity_utils::ActivityLog::discarding(),
        // Read as the server reads them, so the port set above applies.
        aurcache_api::listen::ListenAddresses::from_env().unwrap(),
    ));

    let base = format!("https://localhost:{port}");
    let deadline = Instant::now() + STUCK;
    while fetch_and_pin_ca(&base, None).await.is_err() {
        assert!(
            Instant::now() < deadline,
            "worker protocol listener did not come up on :{port}"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}
