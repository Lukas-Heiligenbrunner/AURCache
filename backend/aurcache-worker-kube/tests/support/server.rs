//! The server's worker protocol listener, in-process, with a worker enrolled
//! through the production client: over an in-memory database and a
//! throwaway CA, as `aurcache-worker/tests/fake_worker.rs` starts it.
//!
//! It reads its port from the process's environment and writes under the
//! process's working directory, so a test file starts one, in one test.

use aurcache_common::build_state::BuildState;
use aurcache_common::worker::{ClaimRequest, MirrorlistPreference};
use aurcache_db::builds;
use aurcache_worker_core::client::{WorkerClient, fetch_and_pin_ca};
use aurcache_worker_core::config::CoreConfig;
use aurcache_worker_core::enroll::ensure_enrolled;
use aurcache_worker_core::executor::ClaimedJob;
use aurcache_worker_core::identity::Identity;
use sea_orm::{ConnectionTrait, DatabaseConnection, EntityTrait};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// The listener, and a worker enrolled with it.
pub struct Server {
    pub db: DatabaseConnection,
    pub repo_root: PathBuf,
    pub client: Arc<WorkerClient>,
    // Dropped last: the listener and the database write under it.
    _dir: tempfile::TempDir,
}

fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

/// An enqueued build of package `p<id>`, whose source is a path that does
/// not exist, so that nothing the server does for it reaches the network.
async fn seed_build(db: &DatabaseConnection, id: i32) {
    let source =
        r#"{"type":"git","url":"/nonexistent-aurcache-kube-test","ref":"HEAD","subfolder":""}"#;
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

impl Server {
    /// Start the listener over a database holding builds 1 and 2 of packages
    /// `p1` and `p2`, enqueued, and enroll a worker with it.
    pub async fn start() -> Self {
        // Under the build's own directory: the server stages what it is sent
        // on disk, and a test may send it a large file.
        let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        let port = free_port();
        // SAFETY: before anything else in this process reads these, by the
        // one test of its file that starts a server.
        unsafe {
            std::env::set_var("AURCACHE_WORKER_PORT", port.to_string());
            std::env::set_var("AURCACHE_TLS_SANS", "localhost");
            std::env::set_var("AURCACHE_ENROLLMENT_TOKEN", "enroll-me");
            std::env::remove_var("AURCACHE_ENROLLMENT_DIR");
            std::env::remove_var("AURCACHE_PREAPPROVED_WORKERS");
            std::env::set_var("AURCACHE_URL", format!("https://localhost:{port}"));
            std::env::set_var("WORKER_DATA_DIR", dir.path().join("worker"));
            std::env::set_var("WORKER_ARCHES", "x86_64");
            std::env::set_var("WORKER_NAME", "kube-test");
        }
        let db = aurcache_db::testing::memory_db().await;
        seed_build(&db, 1).await;
        seed_build(&db, 2).await;

        let ca = aurcache_ca::Ca::load_or_create(&dir.path().join("ca")).unwrap();
        let repo_root = dir.path().join("repo");
        // Runs for as long as the test's runtime does.
        drop(aurcache_api::init::init_worker_api(
            db.clone(),
            ca,
            Arc::new(aurcache_utils::snapshot::SnapshotStore::new()),
            Arc::new(aurcache_utils::repository::Repository::new(&repo_root)),
            aurcache_activitylog::activity_utils::ActivityLog::discarding(),
        ));
        let base = format!("https://localhost:{port}");
        let mut up = false;
        for _ in 0..80 {
            if fetch_and_pin_ca(&base, None).await.is_ok() {
                up = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        assert!(up, "the worker listener did not come up on :{port}");

        let core = CoreConfig::from_env(
            "test".to_string(),
            aurcache_worker_kube::settings::kube_settings(),
        );
        let identity = Identity::load_or_create(&core.data_dir).unwrap();
        let client = ensure_enrolled(&core, &identity, "kubernetes")
            .await
            .expect("the worker should enroll and be approved by its token");
        Self {
            db,
            repo_root,
            client: Arc::new(client),
            _dir: dir,
        }
    }

    /// Claim the next build, as the runner would hand it to the executor.
    pub async fn claim(&self) -> ClaimedJob {
        let job = self
            .client
            .claim(&ClaimRequest {
                mirrorlist: MirrorlistPreference::Server {
                    checksums: Default::default(),
                },
            })
            .await
            .unwrap()
            .expect("a build should be claimable");
        ClaimedJob {
            job,
            mirrorlist: None,
            timeout: None,
            cancel: CancellationToken::new(),
        }
    }

    /// The build's row once it is no longer being published.
    pub async fn settled(&self, id: i32) -> builds::Model {
        for _ in 0..100 {
            let build = builds::Entity::find_by_id(id)
                .one(&self.db)
                .await
                .unwrap()
                .unwrap();
            if build.status != BuildState::Publishing {
                return build;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("build {id} is still publishing");
    }

    /// The names of the files published for the x86_64 repository.
    pub fn published(&self) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(self.repo_root.join("x86_64")) else {
            return Vec::new();
        };
        let mut names: Vec<String> = entries
            .flatten()
            .filter_map(|entry| entry.file_name().into_string().ok())
            .collect();
        names.sort();
        names
    }
}
