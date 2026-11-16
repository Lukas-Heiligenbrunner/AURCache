//! Packages relayed from a pod to the real worker listener.
//!
//! The server's worker protocol listener runs in-process, over an in-memory
//! database and a throwaway CA, and a worker enrolls with it through the
//! production client. The cluster is the fake one, whose `send` yields a
//! package a few bytes at a time. Nothing here is mocked between the
//! dispatcher and the server: the upload is the request the server's route
//! reads, and a build is published by the server's own publishing.

mod support;

use aurcache_build_agent::report::Ended;
use aurcache_common::build_state::BuildState;
use aurcache_common::worker::{BuildOutcome, ClaimRequest, MirrorlistPreference};
use aurcache_db::{builds, files};
use aurcache_worker_container::spec::SpecOptions;
use aurcache_worker_container::{input, spec};
use aurcache_worker_core::build_log::BuildLog;
use aurcache_worker_core::client::{WorkerClient, fetch_and_pin_ca};
use aurcache_worker_core::config::CoreConfig;
use aurcache_worker_core::enroll::ensure_enrolled;
use aurcache_worker_core::executor::ClaimedJob;
use aurcache_worker_core::identity::Identity;
use aurcache_worker_kube::executor::{Build, KubeExecutor};
use aurcache_worker_kube::transfer::Agent;
use sea_orm::{ColumnTrait, ConnectionTrait, DatabaseConnection, EntityTrait, QueryFilter};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use support::{AGENT_IMAGE, FakeCluster, Package, Script, config, pod_name, report, status};
use tokio_util::sync::CancellationToken;

/// The size of the package that must pass without being held.
const LARGE: u64 = 256 << 20;
/// How much the process's peak memory may grow while it passes.
const HEADROOM: u64 = 32 << 20;

/// A minimal but structurally valid `*.pkg.tar.zst`: a zstd-compressed tar
/// with the one `.PKGINFO` the server's publishing reads.
fn package(name: &str, version: &str) -> Package {
    let pkginfo = format!(
        "pkgname = {name}\npkgbase = {name}\npkgver = {version}\npkgdesc = test package\n\
         url = https://example.invalid\nbuilddate = 0\npackager = test <test@localhost>\n\
         size = 0\narch = x86_64\n"
    );
    let mut tar = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_path(".PKGINFO").unwrap();
    header.set_size(pkginfo.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    tar.append(&header, pkginfo.as_bytes()).unwrap();
    let mut encoder = zstd::Encoder::new(Vec::new(), 0).unwrap();
    encoder.write_all(&tar.into_inner().unwrap()).unwrap();
    Package::of(
        &format!("{name}-{version}-x86_64.pkg.tar.zst"),
        &encoder.finish().unwrap(),
    )
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

/// The build's row once it is no longer being published.
async fn settled(db: &DatabaseConnection, id: i32) -> builds::Model {
    for _ in 0..100 {
        let build = builds::Entity::find_by_id(id)
            .one(db)
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

/// The server's worker listener, and a worker enrolled with it.
struct Server {
    db: DatabaseConnection,
    repo_root: PathBuf,
    client: Arc<WorkerClient>,
    // Dropped last: the listener and the database write under it.
    _dir: tempfile::TempDir,
}

impl Server {
    /// Start the listener over a database holding builds 1 and 2, enqueued,
    /// and enroll a worker with it.
    async fn start() -> Self {
        // Under the build's own directory: the server stages what it is sent
        // on disk, which here includes a large file.
        let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        let port = free_port();
        // SAFETY: before anything else in this process reads the
        // environment; this file holds one test, and each test file is a
        // process of its own.
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
    async fn claim(&self) -> ClaimedJob {
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
}

/// The process's peak resident memory so far, in bytes.
fn peak_memory() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    let line = status
        .lines()
        .find(|line| line.starts_with("VmHWM:"))
        .expect("the kernel reports a peak resident size");
    let kib: u64 = line
        .split_whitespace()
        .nth(1)
        .and_then(|figure| figure.parse().ok())
        .expect("the peak is a number of kilobytes");
    kib * 1024
}

/// One listener serves both halves: a build relayed and published, then a
/// large package relayed with nothing of it held.
#[tokio::test]
async fn packages_are_relayed_to_the_server_as_they_are_read() {
    let server = Server::start().await;
    a_build_is_published(&server).await;
    a_large_package_is_not_held(&server).await;
}

/// A build runs through the executor against the real listener: its log
/// arrives, its package is uploaded in small chunks, and the server
/// publishes it.
async fn a_build_is_published(server: &Server) {
    let claimed = server.claim().await;
    assert_eq!(claimed.job.build_id, 1);
    let built = package("p1", "1.0-1");
    let cluster = FakeCluster::new(Script {
        packages: vec![built.clone()],
        chunk: 97,
        ..Script::default()
    });
    let executor = KubeExecutor::new(config(), Arc::clone(&cluster), AGENT_IMAGE.to_string());

    let options = SpecOptions {
        cpus: None,
        keyserver: "hkps://keys.example".to_string(),
        scratch: "/work/scratch".into(),
        credential: None,
    };
    let spec = spec::for_job(&claimed, server.client.repo(), options);
    let job = input::pack(&spec, b"").unwrap();
    let log = BuildLog::open(Arc::clone(&server.client), 1);
    let build = Build {
        claimed: &claimed,
        input: job.clone(),
        log: log.sender(),
        uploads: &*server.client,
    };
    let report = executor.build(build).await.unwrap();
    log.close(Vec::new(), None).await;

    assert_eq!(report.outcome, BuildOutcome::Succeeded, "{report:?}");
    assert_eq!(cluster.received(), [job]);
    assert!(!cluster.has_pod(&pod_name(1)));

    server.client.complete(1, &report).await.unwrap();
    let build = settled(&server.db, 1).await;
    assert_eq!(build.status, BuildState::Successful);
    assert_eq!(build.peak_memory, Some(3 << 30));
    assert_eq!(build.disk_workdir, Some(2 << 30));
    let published = server.repo_root.join("x86_64").join(&built.name);
    let Package {
        content: support::Content::Bytes(bytes),
        ..
    } = &built
    else {
        unreachable!()
    };
    assert_eq!(&std::fs::read(&published).unwrap(), bytes);
    let rows = files::Entity::find()
        .filter(files::Column::PackageId.eq(1))
        .all(&server.db)
        .await
        .unwrap();
    assert!(!rows.is_empty(), "a files row should be recorded");

    let stored = std::fs::read_to_string(aurcache_common::fs::build_log_path("p1", build.number))
        .expect("the build's log should be stored");
    assert!(stored.contains("==> Finished making: hello\n"), "{stored}");
    assert!(
        stored.contains(&format!("[worker] uploading {}\n", built.name)),
        "{stored}"
    );
}

/// 256 MiB go from the pod's `send` into the server's staging directory, and
/// the process, which here is the dispatcher and the server both, does not
/// grow by a fraction of that.
async fn a_large_package_is_not_held(server: &Server) {
    let claimed = server.claim().await;
    assert_eq!(claimed.job.build_id, 2);
    let large = Package::zeros("p2-1.0-1-x86_64.pkg.tar.zst", LARGE);
    let artifact = large.artifact();
    let cluster = FakeCluster::new(Script {
        report: Some(report(Ended::Exited { code: 0 })),
        packages: vec![large],
        chunk: 64 * 1024,
        ..Script::default()
    });
    let name = pod_name(2);
    cluster.leave(&name, status::built(0, "Completed"));
    let agent = Agent {
        cluster: &*cluster,
        pod: &name,
        bin: "/usr/local/bin/aurcache-build-agent",
    };

    let before = peak_memory();
    agent
        .relay(&*server.client, 2, &artifact)
        .await
        .expect("the package should reach the server");
    let grown = peak_memory().saturating_sub(before);

    let staged = server
        .repo_root
        .join(".staging")
        .join("2")
        .join(&artifact.name);
    assert_eq!(std::fs::metadata(&staged).unwrap().len(), LARGE);
    assert!(
        grown < HEADROOM,
        "relaying {LARGE} bytes grew the process's peak memory by {grown} bytes"
    );
}
