//! A worker told to stop, against the real worker listener.
//!
//! The server's worker protocol listener runs in-process, over an in-memory
//! database and a throwaway CA, as `aurcache-worker/tests/fake_worker.rs`
//! starts it, and a worker enrolls with it through the production client.
//! The runner is the production one; the executor is one that builds until
//! it is stopped. What is checked is what the server ends up holding:
//! nothing of this worker's, and a new build of what it was running.

use anyhow::Result;
use aurcache_common::build_state::{BuildState, BuildTrigger, EndReason};
use aurcache_common::worker::CompleteReport;
use aurcache_db::builds;
use aurcache_worker_core::client::{WorkerClient, fetch_and_pin_ca};
use aurcache_worker_core::config::CoreConfig;
use aurcache_worker_core::enroll::ensure_enrolled;
use aurcache_worker_core::executor::{ClaimedJob, Executor};
use aurcache_worker_core::identity::Identity;
use aurcache_worker_core::report;
use aurcache_worker_core::runner::Runner;
use sea_orm::{
    ColumnTrait, ConnectionTrait, DatabaseConnection, EntityTrait, QueryFilter, QueryOrder,
};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

/// Builds until it is told to stop, and says which build it started.
struct UntilStopped {
    started: mpsc::UnboundedSender<i32>,
}

impl Executor for UntilStopped {
    const KIND: &'static str = "docker";

    async fn run_job(&self, _: Arc<WorkerClient>, claimed: ClaimedJob) -> Result<CompleteReport> {
        let _ = self.started.send(claimed.job.build_id);
        claimed.cancel.cancelled().await;
        Ok(report::canceled())
    }

    fn describe_self(&self) -> String {
        "a build that runs until stopped".to_string()
    }
}

fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

/// An enqueued build of package `p<id>`, whose source is a path that does
/// not exist, so that nothing the server does for it reaches the network.
async fn seed_build(db: &DatabaseConnection, id: i32) {
    let source =
        r#"{"type":"git","url":"/nonexistent-aurcache-shutdown-test","ref":"HEAD","subfolder":""}"#;
    db.execute_unprepared(&format!(
        "INSERT INTO packages (id, name, build_flags, source_data, platforms) \
         VALUES ({id}, 'p{id}', '', '{source}', 'x86_64')"
    ))
    .await
    .unwrap();
    db.execute_unprepared(&format!(
        "INSERT INTO builds (id, pkg_id, status, start_time, platform, version) \
         VALUES ({id}, {id}, 3, {id}000, 'x86_64', '1.0')"
    ))
    .await
    .unwrap();
}

/// The listener, over a database holding builds 1 and 2 of packages `p1`
/// and `p2`, enqueued. It reads its port from the process's environment, so
/// this file starts one, in its one test.
async fn start_server(dir: &std::path::Path) -> DatabaseConnection {
    let port = free_port();
    // SAFETY: before anything else in this process reads these, by the one
    // test of this file.
    unsafe {
        std::env::set_var("AURCACHE_WORKER_PORT", port.to_string());
        std::env::set_var("AURCACHE_TLS_SANS", "localhost");
        std::env::set_var("AURCACHE_ENROLLMENT_TOKEN", "enroll-me");
        std::env::remove_var("AURCACHE_ENROLLMENT_DIR");
        std::env::remove_var("AURCACHE_PREAPPROVED_WORKERS");
        std::env::set_var("AURCACHE_URL", format!("https://localhost:{port}"));
        std::env::set_var("WORKER_DATA_DIR", dir.join("worker"));
        std::env::set_var("WORKER_ARCHES", "x86_64");
        std::env::set_var("WORKER_NAME", "shutdown-test");
        std::env::remove_var("WORKER_CONCURRENCY");
    }
    let db = aurcache_db::testing::memory_db().await;
    seed_build(&db, 1).await;
    seed_build(&db, 2).await;

    let ca = aurcache_ca::Ca::load_or_create(&dir.join("ca")).unwrap();
    // Runs for as long as the test's runtime does.
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

/// Every build of package `pkg`, oldest first.
async fn builds_of(db: &DatabaseConnection, pkg: i32) -> Vec<builds::Model> {
    builds::Entity::find()
        .filter(builds::Column::PkgId.eq(pkg))
        .order_by_asc(builds::Column::Id)
        .all(db)
        .await
        .unwrap()
}

/// The worker holds one build when it is told to stop. It reports that build
/// interrupted, so the server queues the package again at once; it claims
/// nothing more, though another build is waiting; and its loop ends.
#[tokio::test]
async fn a_stopped_worker_hands_its_build_back_and_claims_nothing_more() {
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    std::env::set_current_dir(dir.path()).unwrap();
    let db = start_server(dir.path()).await;
    let core = CoreConfig::from_env("test".to_string(), Vec::new());
    assert_eq!(core.tunables.concurrency, 1, "one build at a time");
    let identity = Identity::load_or_create(&core.data_dir).unwrap();
    let client = ensure_enrolled(&core, &identity, UntilStopped::KIND)
        .await
        .unwrap();
    let (started, mut building) = mpsc::unbounded_channel();
    let runner = Runner::new(core, client, UntilStopped { started });
    let running = tokio::spawn(Arc::clone(&runner).run());

    let held = tokio::time::timeout(Duration::from_secs(30), building.recv())
        .await
        .expect("the worker should claim a build")
        .unwrap();
    runner.stop();
    tokio::time::timeout(Duration::from_secs(30), running)
        .await
        .expect("a stopped worker's loop should end")
        .unwrap()
        .unwrap();
    assert!(
        building.try_recv().is_err(),
        "nothing is claimed once the worker is stopping"
    );

    // Builds 1 and 2 are of packages 1 and 2: the one it held, and the other.
    let other = 3 - held;
    let of_held = builds_of(&db, held).await;
    assert_eq!(of_held.len(), 2, "the server queued the package again");
    assert_eq!(of_held[0].status, BuildState::Failed);
    assert_eq!(of_held[0].end_reason, Some(EndReason::Interrupted));
    assert_eq!(of_held[1].status, BuildState::Enqueued);
    assert_eq!(of_held[1].trigger, BuildTrigger::TimeoutRetry);
    assert_eq!(of_held[1].worker_id, None);
    let of_other = builds_of(&db, other).await;
    assert_eq!(of_other.len(), 1);
    assert_eq!(of_other[0].status, BuildState::Enqueued);
    assert_eq!(of_other[0].worker_id, None, "never claimed");
}
