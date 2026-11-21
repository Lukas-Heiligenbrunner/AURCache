//! A worker told to stop, against the real worker listener. The runner is
//! `aurcache-worker-core`'s, which every worker runs; the executor builds
//! until it is stopped. The test checks what the server ends up holding:
//! nothing of this worker's, and a new build of what it was running.

mod common;

use anyhow::Result;
use aurcache_common::build_state::{BuildState, BuildTrigger, EndReason};
use aurcache_common::worker::CompleteReport;
use aurcache_db::builds;
use aurcache_worker_core::client::WorkerClient;
use aurcache_worker_core::config::CoreConfig;
use aurcache_worker_core::enroll::ensure_enrolled;
use aurcache_worker_core::executor::{ClaimedJob, Executor};
use aurcache_worker_core::identity::Identity;
use aurcache_worker_core::report;
use aurcache_worker_core::runner::Runner;
use common::{STUCK, seed_build, start_listener};
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, QueryOrder};
use std::sync::Arc;
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
    let db = aurcache_db::testing::memory_db().await;
    seed_build(&db, 1, "x86_64", 1000).await;
    seed_build(&db, 2, "x86_64", 2000).await;
    let repo = Arc::new(aurcache_utils::repository::Repository::new(
        dir.path().join("repo"),
    ));
    start_listener(dir.path(), &db, repo, "shutdown-test").await;
    let core = CoreConfig::from_env("test".to_string(), Vec::new());
    assert_eq!(core.tunables.concurrency, 1, "one build at a time");
    let identity = Identity::load_or_create(&core.data_dir).unwrap();
    let client = ensure_enrolled(&core, &identity, UntilStopped::KIND)
        .await
        .unwrap();
    let (started, mut building) = mpsc::unbounded_channel();
    let runner = Runner::new(core, client, UntilStopped { started });
    let running = tokio::spawn(Arc::clone(&runner).run());

    let held = tokio::time::timeout(STUCK, building.recv())
        .await
        .expect("the worker should claim a build")
        .unwrap();
    runner.stop();
    tokio::time::timeout(STUCK, running)
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
