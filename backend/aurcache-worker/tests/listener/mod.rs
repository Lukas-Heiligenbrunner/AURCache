//! The server's worker protocol listener, in-process, over an in-memory
//! database and a throwaway CA, for a test to enroll a worker with through
//! the production client.
//!
//! It reads its port from the process's environment, as the server does, so
//! a test file that starts one starts it in its one test.

use aurcache_worker_core::client::fetch_and_pin_ca;
use sea_orm::DatabaseConnection;
use std::sync::Arc;
use std::time::Duration;

fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

/// The listener, with an empty database, and the environment set for a
/// worker named `name` building `arches` (comma-separated) to enroll with
/// it. It runs for as long as the test's runtime does.
pub async fn start(dir: &std::path::Path, name: &str, arches: &str) -> DatabaseConnection {
    let port = free_port();
    // SAFETY: before anything else in this process reads these, by the one
    // test of the file that starts the listener.
    unsafe {
        std::env::set_var("AURCACHE_WORKER_PORT", port.to_string());
        std::env::set_var("AURCACHE_TLS_SANS", "localhost");
        std::env::set_var("AURCACHE_ENROLLMENT_TOKEN", "enroll-me");
        std::env::remove_var("AURCACHE_ENROLLMENT_DIR");
        std::env::remove_var("AURCACHE_PREAPPROVED_WORKERS");
        std::env::set_var("AURCACHE_URL", format!("https://localhost:{port}"));
        std::env::set_var("WORKER_DATA_DIR", dir.join("worker"));
        std::env::set_var("WORKER_ARCHES", arches);
        std::env::set_var("WORKER_NAME", name);
        std::env::remove_var("WORKER_CONCURRENCY");
    }
    let db = aurcache_db::testing::memory_db().await;

    let ca = aurcache_ca::Ca::load_or_create(&dir.join("ca")).unwrap();
    drop(aurcache_api::init::init_worker_api(
        db.clone(),
        ca,
        Arc::new(aurcache_utils::snapshot::SnapshotStore::new()),
        Arc::new(aurcache_utils::repository::Repository::new(
            dir.join("repo"),
        )),
        aurcache_activitylog::activity_utils::ActivityLog::discarding(),
        // Read as the server reads them, so the port set above applies.
        aurcache_api::listen::ListenAddresses::from_env().unwrap(),
    ));
    let base = format!("https://localhost:{port}");
    for _ in 0..80 {
        if fetch_and_pin_ca(&base, None).await.is_ok() {
            return db;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    panic!("the worker listener did not come up on :{port}");
}
