//! A worker whose executor serves fewer architectures than it is configured
//! for registers again with only those, against the real worker listener
//! (`listener`), and its executor is told what was registered.

mod listener;

use anyhow::Result;
use aurcache_common::worker::CompleteReport;
use aurcache_db::workers;
use aurcache_worker_core::client::WorkerClient;
use aurcache_worker_core::config::CoreConfig;
use aurcache_worker_core::enroll::ensure_enrolled;
use aurcache_worker_core::executor::{ClaimedJob, Executor, Serving};
use aurcache_worker_core::identity::Identity;
use aurcache_worker_core::runner::Runner;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

/// Serves aarch64 alone, claims nothing, and passes on what it is told was
/// registered.
struct Narrowed {
    registered: mpsc::UnboundedSender<Vec<String>>,
}

impl Executor for Narrowed {
    const KIND: &'static str = "docker";

    async fn run_job(&self, _: Arc<WorkerClient>, _: ClaimedJob) -> Result<CompleteReport> {
        unreachable!("this executor claims nothing")
    }

    async fn ready_for_work(&self, _claimed: usize) -> bool {
        false
    }

    fn serving(&self, _arches: &[String]) -> Serving {
        Serving::Only(vec!["aarch64".to_string()])
    }

    fn registered(&self, arches: &[String]) {
        let _ = self.registered.send(arches.to_vec());
    }

    fn describe_self(&self) -> String {
        "an executor that serves aarch64 alone".to_string()
    }
}

/// The first heartbeat registers the worker again with the architectures
/// its executor serves, the server's row then holds only those, and the
/// executor hears what was registered.
#[tokio::test]
async fn a_narrowed_executor_is_registered_with_what_it_serves() {
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    std::env::set_current_dir(dir.path()).unwrap();
    let db = listener::start(dir.path(), "registration-test", "x86_64,aarch64").await;
    let core = CoreConfig::from_env("test".to_string(), Vec::new());
    assert_eq!(core.arches, ["x86_64", "aarch64"]);
    let identity = Identity::load_or_create(&core.data_dir).unwrap();
    let client = ensure_enrolled(&core, &identity, Narrowed::KIND)
        .await
        .unwrap();
    let worker = || async {
        workers::Entity::find()
            .filter(workers::Column::Name.eq("registration-test"))
            .one(&db)
            .await
            .unwrap()
            .unwrap()
    };
    assert_eq!(worker().await.arches.as_slice(), ["x86_64", "aarch64"]);

    let (registered, mut told) = mpsc::unbounded_channel();
    let runner = Runner::new(core, client, Narrowed { registered });
    let running = tokio::spawn(Arc::clone(&runner).run());
    let arches = tokio::time::timeout(Duration::from_secs(30), told.recv())
        .await
        .expect("the worker should register again")
        .unwrap();
    assert_eq!(arches, ["aarch64"]);
    assert_eq!(worker().await.arches.as_slice(), ["aarch64"]);

    runner.stop();
    tokio::time::timeout(Duration::from_secs(30), running)
        .await
        .expect("a stopped worker's loop should end")
        .unwrap()
        .unwrap();
}
