//! The dispatcher against a real cluster: real pods, the real agent in them,
//! and for the build that succeeds the real worker listener, in-process.
//!
//! Ignored unless asked for, since they need a cluster:
//!
//! ```sh
//! AURCACHE_KUBE_TESTS=1 KUBECONFIG=<the dispatcher's ServiceAccount> \
//!     AURCACHE_KUBE_TEST_AGENT_IMAGE=<an image the nodes have, with the agent> \
//!     cargo test -p aurcache-worker-kube --test live -- --include-ignored --test-threads=1
//! ```
//!
//! The namespace is `AURCACHE_KUBE_TEST_NAMESPACE` (default
//! `aurcache-builds`), set up as `kubernetes/kube-worker/` sets it up. The
//! agent image has the statically linked agent at
//! `/usr/local/bin/aurcache-build-agent` and is not pulled, so it is loaded
//! onto the nodes beforehand. `AURCACHE_KUBE_TEST_BUILDER_IMAGE` names the
//! builder image (default: the published one, which the nodes pull, and whose
//! build then upgrades it from the Arch mirrors); `AURCACHE_KUBE_TEST_BARE_IMAGE`
//! an image with no pacman in it (default `busybox:1.37`).

mod support;

use aurcache_common::build_state::BuildState;
use aurcache_common::worker::BuildOutcome;
use aurcache_worker_container::spec::SpecOptions;
use aurcache_worker_container::{input, spec};
use aurcache_worker_core::build_log::BuildLog;
use aurcache_worker_core::executor::{ClaimedJob, Executor};
use aurcache_worker_kube::cluster::Cluster;
use aurcache_worker_kube::config::{AgentImage, Config, PodExtras, Tunables};
use aurcache_worker_kube::executor::{Build, KubeExecutor};
use aurcache_worker_kube::kube_cluster::KubeCluster;
use aurcache_worker_kube::pod;
use sea_orm::EntityTrait;
use std::io::Write;
use std::sync::Arc;
use std::time::Duration;
use support::server::Server;
use support::{Sink, claimed};

const BUILDER: &str = "ghcr.io/lukas-heiligenbrunner/aurcache-builder-v2:latest";
const BARE: &str = "busybox:1.37";
/// How long a deleted pod may take to leave the watch.
const GONE: Duration = Duration::from_secs(60);

fn enabled() -> bool {
    let on = std::env::var("AURCACHE_KUBE_TESTS").is_ok_and(|v| v == "1");
    if !on {
        eprintln!("skipped: set AURCACHE_KUBE_TESTS=1 and KUBECONFIG to run against a cluster");
    }
    on
}

fn variable(name: &str, default: Option<&str>) -> String {
    std::env::var(name)
        .ok()
        .or_else(|| default.map(str::to_string))
        .unwrap_or_else(|| panic!("{name} must be set for the live tests"))
}

/// A dispatcher's configuration for the cluster under test. `dispatcher`
/// keeps one test's pods apart from another's.
fn config(dispatcher: &str, builder_image: String, schedule_timeout: Duration) -> Config {
    Config {
        namespace: variable("AURCACHE_KUBE_TEST_NAMESPACE", Some("aurcache-builds")),
        dispatcher: dispatcher.to_string(),
        builder_image,
        pull_policy: "IfNotPresent".to_string(),
        agent_image: AgentImage::Given(variable("AURCACHE_KUBE_TEST_AGENT_IMAGE", None)),
        credential_secret: None,
        user_namespaces: false,
        extras: PodExtras::default(),
        network_probe: false,
        tunables: Tunables {
            memory: Some(2 << 30),
            cpus: Some(2.0),
            disk: None,
            schedule_timeout,
            keyserver: "hkps://keyserver.ubuntu.com".to_string(),
        },
    }
}

/// A dispatcher connected to the cluster under test.
struct Dispatcher {
    cluster: Arc<KubeCluster>,
    executor: KubeExecutor<KubeCluster>,
    cfg: Config,
}

impl Dispatcher {
    async fn connect(cfg: Config) -> Self {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let cluster = Arc::new(
            KubeCluster::connect(&cfg.namespace, &cfg.dispatcher)
                .await
                .unwrap(),
        );
        let AgentImage::Given(agent_image) = cfg.agent_image.clone() else {
            unreachable!("the live configuration names its agent image");
        };
        let executor = KubeExecutor::new(cfg.clone(), Arc::clone(&cluster), agent_image);
        // Whatever an earlier, failed run of this test left.
        executor.sweep().await.unwrap();
        Self {
            cluster,
            executor,
            cfg,
        }
    }

    /// Wait for the pod of `build_id` to be gone from the cluster.
    async fn gone(&self, build_id: i32) {
        let name = pod::name(&self.cfg.dispatcher, build_id);
        let mut moved = self.cluster.generation();
        let leaving = async {
            while self.cluster.pod(&name).is_some() {
                moved.changed().await.unwrap();
            }
        };
        tokio::time::timeout(GONE, leaving)
            .await
            .unwrap_or_else(|_| panic!("pod {name} was not deleted"));
    }
}

/// The server's source archive for a package: `<pkgbase>/PKGBUILD`, gzipped.
fn source(pkgbase: &str, pkgbuild: &str) -> Vec<u8> {
    let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let mut tar = tar::Builder::new(gz);
    let mut header = tar::Header::new_gnu();
    header.set_size(pkgbuild.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    tar.append_data(
        &mut header,
        format!("{pkgbase}/PKGBUILD"),
        pkgbuild.as_bytes(),
    )
    .unwrap();
    let mut gz = tar.into_inner().unwrap();
    gz.flush().unwrap();
    gz.finish().unwrap()
}

/// A package with no sources and no dependencies, whose `build()` is `body`.
fn pkgbuild(pkgname: &str, body: &str) -> String {
    format!(
        "pkgname={pkgname}\npkgver=1.0\npkgrel=1\npkgdesc='built in a pod'\narch=('x86_64')\n\
         license=('MIT')\nbuild() {{\n  {body}\n}}\n\
         package() {{\n  install -Dm644 /dev/null \"$pkgdir/usr/share/{pkgname}/built\"\n}}\n"
    )
}

/// The job as `receive` reads it, for `claimed` built from `pkgbuild`.
fn job(claimed: &ClaimedJob, cfg: &Config, pkgbuild: &str) -> Vec<u8> {
    let options = SpecOptions {
        cpus: cfg.tunables.cpus,
        keyserver: cfg.tunables.keyserver.clone(),
        scratch: "/work/scratch".into(),
        credential: None,
    };
    let spec = spec::for_job(claimed, None, options);
    input::pack(&spec, &source(&claimed.job.pkgbase, pkgbuild)).unwrap()
}

/// A stock `pacman.conf`, for a job no server rendered one for.
const PACMAN_CONF: &str = "[options]\nHoldPkg = pacman glibc\nArchitecture = auto\n\
    CheckSpace\nSigLevel = Required DatabaseOptional\nLocalFileSigLevel = Optional\n\
    [core]\nInclude = /etc/pacman.d/mirrorlist\n[extra]\nInclude = /etc/pacman.d/mirrorlist\n";

/// What a build without a server came to.
struct Ran {
    report: aurcache_common::worker::CompleteReport,
    log: String,
}

impl Dispatcher {
    /// Build `claimed` from `input` with no server behind it: the log is
    /// collected, and `watch` sees it grow.
    async fn run(
        &self,
        claimed: &ClaimedJob,
        input: Vec<u8>,
        sink: &Sink,
        watch: impl Fn(&str),
    ) -> Ran {
        let (log, mut lines) = tokio::sync::mpsc::channel(16);
        let build = Build {
            claimed,
            input,
            log,
            uploads: sink,
        };
        let collect = async {
            let mut text = String::new();
            while let Some(chunk) = lines.recv().await {
                print!("{chunk}");
                text.push_str(&chunk);
                watch(&text);
            }
            text
        };
        let (report, log) = tokio::join!(self.executor.build(build), collect);
        Ran {
            report: report.expect("the build should end in a report"),
            log,
        }
    }
}

/// A package builds in a pod from the published builder image, under the
/// namespace's Pod Security level and NetworkPolicy, and what it made goes
/// through the server's upload route into the repository.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a cluster: AURCACHE_KUBE_TESTS=1"]
async fn a_package_builds_in_a_pod_and_is_published() {
    if !enabled() {
        return;
    }
    let server = Server::start().await;
    let builder = variable("AURCACHE_KUBE_TEST_BUILDER_IMAGE", Some(BUILDER));
    let dispatcher = Dispatcher::connect(config(
        "11fe00000000b001",
        builder,
        Duration::from_secs(600),
    ))
    .await;
    assert!(
        dispatcher.executor.ready_for_work(0).await,
        "the cluster should admit a build pod"
    );

    let claimed = server.claim().await;
    assert_eq!(claimed.job.pkgbase, "p1");
    let options = SpecOptions {
        cpus: dispatcher.cfg.tunables.cpus,
        keyserver: dispatcher.cfg.tunables.keyserver.clone(),
        scratch: "/work/scratch".into(),
        credential: None,
    };
    // Without the repository section: the server is this process, which no
    // pod can reach.
    let spec = spec::for_job(&claimed, None, options);
    let recipe = pkgbuild("p1", "echo 'hello from the pod'");
    let input = input::pack(&spec, &source("p1", &recipe)).unwrap();
    let log = BuildLog::open(Arc::clone(&server.client), 1);
    let build = Build {
        claimed: &claimed,
        input,
        log: log.sender(),
        uploads: &*server.client,
    };
    let report = dispatcher.executor.build(build).await.unwrap();
    log.close(Vec::new(), None).await;
    let build = aurcache_db::builds::Entity::find_by_id(1)
        .one(&server.db)
        .await
        .unwrap()
        .unwrap();
    let stored = std::fs::read_to_string(aurcache_common::fs::build_log_path("p1", build.number))
        .unwrap_or_default();
    println!("{stored}");

    assert_eq!(report.outcome, BuildOutcome::Succeeded, "{report:?}");
    assert!(report.peak_memory_bytes.is_some_and(|peak| peak > 0));
    assert!(report.disk_usage.is_some());
    assert!(stored.contains("hello from the pod"), "{stored}");
    assert!(stored.contains("[worker] uploading p1-1.0-1-x86_64.pkg.tar."));
    dispatcher.gone(1).await;

    server.client.complete(1, &report).await.unwrap();
    assert_eq!(server.settled(1).await.status, BuildState::Successful);
    let published = server.published();
    assert!(
        published
            .iter()
            .any(|name| name.starts_with("p1-1.0-1-x86_64.pkg.tar.")),
        "{published:?}"
    );
}

/// A cancel while the build container runs deletes the pod and reports the
/// build as canceled.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a cluster: AURCACHE_KUBE_TESTS=1"]
async fn a_cancel_deletes_the_running_pod() {
    if !enabled() {
        return;
    }
    let builder = variable("AURCACHE_KUBE_TEST_BUILDER_IMAGE", Some(BUILDER));
    let dispatcher = Dispatcher::connect(config(
        "11fe00000000b002",
        builder,
        Duration::from_secs(600),
    ))
    .await;
    let mut claimed = claimed(2, Some(Duration::from_secs(1800)));
    claimed.job.pacman_conf = PACMAN_CONF.to_string();
    let input = job(&claimed, &dispatcher.cfg, &pkgbuild("hello", "sleep 900"));
    let cancel = claimed.cancel.clone();
    let sink = Sink::default();
    let ran = dispatcher
        .run(&claimed, input, &sink, |log| {
            // The agent's own first line: the build container is running.
            if log.contains("[agent]") {
                cancel.cancel();
            }
        })
        .await;
    assert_eq!(
        ran.report.outcome,
        BuildOutcome::Canceled,
        "{:?}",
        ran.report
    );
    assert!(
        ran.log.contains("[worker] the build is running"),
        "{}",
        ran.log
    );
    assert!(sink.taken().is_empty());
    dispatcher.gone(2).await;
}

/// A pod no node can take holds claims back while it waits, and is given
/// back once the schedule timeout has passed.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a cluster: AURCACHE_KUBE_TESTS=1"]
async fn a_pod_no_node_takes_is_given_back() {
    if !enabled() {
        return;
    }
    let bare = variable("AURCACHE_KUBE_TEST_BARE_IMAGE", Some(BARE));
    let dispatcher =
        Dispatcher::connect(config("11fe00000000b003", bare, Duration::from_secs(20))).await;
    // The cluster under test has no node of this architecture.
    let mut claimed = claimed(3, None);
    claimed.job.arch = "riscv64".to_string();
    let input = job(&claimed, &dispatcher.cfg, &pkgbuild("hello", "true"));
    let sink = Sink::default();
    let held_back = async {
        tokio::time::sleep(Duration::from_secs(10)).await;
        !dispatcher.executor.ready_for_work(1).await
    };
    let (ran, held_back) = tokio::join!(dispatcher.run(&claimed, input, &sink, |_| {}), held_back);
    assert!(held_back, "no claim while a pod is unschedulable");
    assert_eq!(ran.report.outcome, BuildOutcome::Interrupted);
    let reason = ran.report.reason.unwrap();
    assert!(
        reason.starts_with(
            "no node could take the build within 20s (WORKER_KUBE_SCHEDULE_TIMEOUT): "
        ),
        "{reason}"
    );
    assert!(
        ran.log
            .contains("[worker] no node can take the build pod yet")
    );
    dispatcher.gone(3).await;
    assert!(dispatcher.executor.ready_for_work(0).await);
}

/// A builder image the agent cannot prepare is the worker's problem: the
/// agent's report says which step failed, and nothing is uploaded.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a cluster: AURCACHE_KUBE_TESTS=1"]
async fn an_image_without_pacman_is_an_interruption_the_agent_reports() {
    if !enabled() {
        return;
    }
    let bare = variable("AURCACHE_KUBE_TEST_BARE_IMAGE", Some(BARE));
    let dispatcher =
        Dispatcher::connect(config("11fe00000000b004", bare, Duration::from_secs(600))).await;
    let mut claimed = claimed(4, Some(Duration::from_secs(600)));
    claimed.job.pacman_conf = PACMAN_CONF.to_string();
    let input = job(&claimed, &dispatcher.cfg, &pkgbuild("hello", "true"));
    let sink = Sink::default();
    let ran = dispatcher.run(&claimed, input, &sink, |_| {}).await;
    assert_eq!(
        ran.report.outcome,
        BuildOutcome::Interrupted,
        "{:?}",
        ran.report
    );
    let reason = ran.report.reason.unwrap();
    assert!(
        reason.starts_with("the build could not start: "),
        "{reason}"
    );
    assert!(ran.log.contains("[agent]"), "{}", ran.log);
    assert!(sink.taken().is_empty());
    dispatcher.gone(4).await;
}
