//! [`KubeCluster`] against a real API server. Ignored by default, and skipped unless
//! `AURCACHE_KUBE_TESTS=1`; they talk to whatever `KUBECONFIG` points at, in
//! `AURCACHE_KUBE_TEST_NAMESPACE` (default `aurcache-builds`), and run their
//! pods from `AURCACHE_KUBE_TEST_IMAGE`, any image with `sh` and coreutils
//! (default `busybox:1.37`). The sweep test deletes every build pod in that
//! namespace, so point them only at a cluster or namespace with no worker of
//! its own.
//!
//! The namespace is the one `kubernetes/kube-worker/` sets up, with the
//! credentials of its `aurcache-kube-worker` ServiceAccount, so they show that
//! the Role there is enough:
//!
//! ```text
//! kubectl create namespace aurcache
//! kubectl apply -f kubernetes/kube-worker/namespace.yaml -f kubernetes/kube-worker/rbac.yaml
//! kubectl -n aurcache create token aurcache-kube-worker   # into a kubeconfig
//! AURCACHE_KUBE_TESTS=1 KUBECONFIG=... cargo test -p aurcache-worker-kube \
//!     --test live -- --ignored --test-threads=1 --nocapture
//! ```

use aurcache_worker_kube::cluster::{Cluster, CreateError, ExecEnd, PROMPT, Stdin, Sweep};
use aurcache_worker_kube::kube_cluster::KubeCluster;
use aurcache_worker_kube::pod;
use k8s_openapi::api::core::v1::Pod;
use std::time::{Duration, SystemTime};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const RUNNING: Duration = Duration::from_secs(180);

fn enabled() -> bool {
    let on = std::env::var("AURCACHE_KUBE_TESTS").is_ok_and(|v| v == "1");
    if !on {
        eprintln!("skipped: set AURCACHE_KUBE_TESTS=1 and KUBECONFIG to run against a cluster");
    }
    on
}

fn namespace() -> String {
    std::env::var("AURCACHE_KUBE_TEST_NAMESPACE").unwrap_or_else(|_| "aurcache-builds".to_owned())
}

fn image() -> String {
    std::env::var("AURCACHE_KUBE_TEST_IMAGE").unwrap_or_else(|_| "busybox:1.37".to_owned())
}

/// A name unlikely to clash with an earlier run's: `live-<tag>-<clock>`.
fn unique(tag: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .subsec_nanos();
    format!("live-{tag}-{nanos:08x}")
}

async fn connect(dispatcher: &str) -> KubeCluster {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    KubeCluster::connect(&namespace(), dispatcher)
        .await
        .unwrap()
}

/// A pod of `dispatcher` running `script`, never restarted.
fn shell_pod(dispatcher: &str, name: &str, script: &str) -> Pod {
    serde_json::from_value(serde_json::json!({
        "metadata": { "name": name, "labels": pod::labels(dispatcher) },
        "spec": {
            "restartPolicy": "Never",
            "terminationGracePeriodSeconds": 0,
            "containers": [{
                "name": "main",
                "image": image(),
                "imagePullPolicy": "IfNotPresent",
                "command": ["sh", "-c", script],
            }],
        },
    }))
    .unwrap()
}

fn phase(pod: &Pod) -> &str {
    pod.status
        .as_ref()
        .and_then(|status| status.phase.as_deref())
        .unwrap_or("")
}

/// Wait until the watch shows `name` as `wanted` says, or fail after `limit`.
async fn until(
    cluster: &KubeCluster,
    name: &str,
    limit: Duration,
    wanted: impl Fn(Option<&Pod>) -> bool,
) {
    let mut moved = cluster.generation();
    let deadline = tokio::time::Instant::now() + limit;
    while !wanted(cluster.pod(name).as_deref()) {
        let waited = tokio::time::timeout_at(deadline, moved.changed()).await;
        assert!(waited.is_ok(), "the watch never showed {name} as wanted");
    }
}

async fn running(cluster: &KubeCluster, dispatcher: &str, name: &str) {
    cluster
        .create(&shell_pod(dispatcher, name, "sleep 3600"))
        .await
        .unwrap();
    until(cluster, name, RUNNING, |pod| {
        pod.is_some_and(|pod| phase(pod) == "Running")
    })
    .await;
}

struct Ran {
    stdout: Vec<u8>,
    end: ExecEnd,
}

async fn run(cluster: &KubeCluster, pod: &str, argv: &[&str]) -> Ran {
    let mut exec = cluster
        .exec(pod, "main", argv, Stdin::Closed)
        .await
        .unwrap();
    let mut stdout = Vec::new();
    let _ = exec.stdout.read_to_end(&mut stdout).await;
    Ran {
        stdout,
        end: exec.ended.await,
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a cluster: AURCACHE_KUBE_TESTS=1"]
async fn a_pod_is_watched_to_its_end() {
    if !enabled() {
        return;
    }
    let dispatcher = unique("watch");
    let cluster = connect(&dispatcher).await;
    assert!(cluster.pods().is_empty());
    let name = unique("done");
    let pod = shell_pod(&dispatcher, &name, "echo first; echo second");
    cluster.create(&pod).await.unwrap();
    assert!(matches!(
        cluster.create(&pod).await,
        Err(CreateError::Refused { .. })
    ));
    until(&cluster, &name, RUNNING, |pod| {
        pod.is_some_and(|pod| phase(pod) == "Succeeded")
    })
    .await;
    assert_eq!(cluster.pods().len(), 1);
    assert_eq!(
        cluster.log_tail(&name, "main", 1).await.unwrap(),
        "second\n"
    );
    cluster.delete(&name, PROMPT).await.unwrap();
    until(&cluster, &name, RUNNING, |pod| pod.is_none()).await;
    cluster.delete(&name, PROMPT).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a cluster: AURCACHE_KUBE_TESTS=1"]
async fn exec_streams_and_reports() {
    if !enabled() {
        return;
    }
    let dispatcher = unique("exec");
    let cluster = connect(&dispatcher).await;
    let name = unique("sleep");
    running(&cluster, &dispatcher, &name).await;

    // stdin in, stdout back, and a dropped stdin is its end.
    let sent: Vec<u8> = (0..4 * 1024 * 1024u32).map(|i| (i % 251) as u8).collect();
    let mut exec = cluster
        .exec(&name, "main", &["cat"], Stdin::Piped)
        .await
        .unwrap();
    let mut stdin = exec.stdin.take().unwrap();
    let input = sent.clone();
    let writing = tokio::spawn(async move {
        stdin.write_all(&input).await.unwrap();
    });
    let mut echoed = Vec::new();
    exec.stdout.read_to_end(&mut echoed).await.unwrap();
    writing.await.unwrap();
    // A stream can be cut short and still end in success: see
    // `ExecEnd::Succeeded`. What arrived is still the start of it.
    eprintln!(
        "cat: {} bytes short",
        sent.len().saturating_sub(echoed.len())
    );
    assert!(!echoed.is_empty(), "nothing came back through cat");
    assert!(echoed == sent[..echoed.len()]);
    assert_eq!(exec.ended.await, ExecEnd::Succeeded);

    let failed = run(&cluster, &name, &["false"]).await;
    let ExecEnd::Failed { message } = failed.end else {
        panic!("false ended {:?}", failed.end);
    };
    assert!(message.contains("exit code 1"), "{message}");

    let loud = run(
        &cluster,
        &name,
        &["sh", "-c", "echo out; echo oops >&2; exit 3"],
    )
    .await;
    assert_eq!(loud.stdout, b"out\n");
    let ExecEnd::Failed { message } = loud.end else {
        panic!("exit 3 ended {:?}", loud.end);
    };
    assert!(
        message.contains("exit code 3") && message.ends_with("stderr: oops"),
        "{message}"
    );

    // Far more stderr than its pipe holds, written before stdout's end.
    let noisy = run(
        &cluster,
        &name,
        &[
            "sh",
            "-c",
            "head -c 20000000 /dev/zero | tr '\\0' e >&2; echo done",
        ],
    )
    .await;
    assert_eq!(noisy.stdout, b"done\n");
    assert_eq!(noisy.end, ExecEnd::Succeeded);

    cluster.delete(&name, PROMPT).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a cluster: AURCACHE_KUBE_TESTS=1"]
async fn an_exec_ends_with_its_pod() {
    if !enabled() {
        return;
    }
    let dispatcher = unique("gone");
    let cluster = connect(&dispatcher).await;
    let name = unique("doomed");
    running(&cluster, &dispatcher, &name).await;
    let mut exec = cluster
        .exec(
            &name,
            "main",
            &["sh", "-c", "echo started; exec sleep 3600"],
            Stdin::Closed,
        )
        .await
        .unwrap();
    let mut line = [0; 8];
    exec.stdout.read_exact(&mut line).await.unwrap();
    assert_eq!(&line, b"started\n");
    cluster.delete(&name, Duration::from_secs(1)).await.unwrap();
    let mut rest = Vec::new();
    let read = tokio::time::timeout(RUNNING, exec.stdout.read_to_end(&mut rest)).await;
    assert!(read.is_ok(), "stdout outlived its pod");
    let end = tokio::time::timeout(RUNNING, exec.ended).await.unwrap();
    eprintln!("an exec in a deleted pod: stdout {read:?}, ended {end:?}");
    assert_ne!(end, ExecEnd::Succeeded);
}

/// A namespace sweep deletes every dispatcher's pods in the namespace,
/// those of any other test running at the same time included: run the
/// tests against a cluster one at a time (`--test-threads=1`).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a cluster: AURCACHE_KUBE_TESTS=1"]
async fn sweep_deletes_its_own_or_every_dispatchers_pods_and_nothing_else() {
    if !enabled() {
        return;
    }
    let theirs = unique("theirs");
    let other = connect(&theirs).await;
    // Unmanaged (no `managed-by` label), though the other's watch shows it.
    // No sweep deletes it, so this test does, however the checks end.
    let kept = unique("kept");
    let mut unmanaged = shell_pod(&theirs, &kept, "sleep 3600");
    unmanaged.metadata.labels = Some(std::collections::BTreeMap::from([(
        "aurcache.io/dispatcher".to_owned(),
        theirs.clone(),
    )]));
    other.create(&unmanaged).await.unwrap();
    let checked = tokio::spawn(sweep_checks(theirs, kept.clone())).await;
    other.delete(&kept, PROMPT).await.unwrap();
    if let Err(failed) = checked {
        std::panic::resume_unwind(failed.into_panic());
    }
}

/// What [`sweep_deletes_its_own_or_every_dispatchers_pods_and_nothing_else`]
/// checks, beside the pod `kept` of dispatcher `theirs`, which no sweep
/// may delete.
async fn sweep_checks(theirs: String, kept: String) {
    let ours = unique("ours");
    let cluster = connect(&ours).await;
    let other = connect(&theirs).await;
    // What an earlier test left, which the counts below would include:
    // being deleted from here on, it is not counted again.
    cluster.sweep(Sweep::Namespace).await.unwrap();
    for _ in 0..3 {
        cluster
            .create(&shell_pod(&ours, &unique("swept"), "sleep 3600"))
            .await
            .unwrap();
    }
    // Another dispatcher's, whose identity is gone.
    let lost = unique("lost");
    cluster
        .create(&shell_pod(&theirs, &lost, "sleep 3600"))
        .await
        .unwrap();
    until(&other, &lost, RUNNING, |pod| pod.is_some()).await;
    until(&other, &kept, RUNNING, |pod| pod.is_some()).await;

    assert_eq!(cluster.sweep(Sweep::Own).await.unwrap(), 3);
    assert!(
        other.pod(&lost).is_some(),
        "another dispatcher's pod is left"
    );
    // Ours are still terminating: they are not counted again.
    assert_eq!(cluster.sweep(Sweep::Namespace).await.unwrap(), 1);
    let mut moved = cluster.generation();
    while !cluster.pods().is_empty() {
        tokio::time::timeout(RUNNING, moved.changed())
            .await
            .unwrap()
            .unwrap();
    }
    until(&other, &lost, RUNNING, |pod| pod.is_none()).await;
    assert!(other.pod(&kept).is_some());
    assert_eq!(cluster.sweep(Sweep::Namespace).await.unwrap(), 0);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a cluster: AURCACHE_KUBE_TESTS=1"]
async fn a_dry_run_refuses_what_pod_security_forbids() {
    if !enabled() {
        return;
    }
    let dispatcher = unique("psa");
    let cluster = connect(&dispatcher).await;
    let name = unique("priv");
    let mut privileged = shell_pod(&dispatcher, &name, "true");
    let container = &mut privileged.spec.as_mut().unwrap().containers[0];
    container.security_context = Some(k8s_openapi::api::core::v1::SecurityContext {
        privileged: Some(true),
        ..Default::default()
    });
    let message = format!("{:#}", cluster.admits(&privileged).await.unwrap_err());
    assert!(message.contains("privileged"), "{message}");
    assert!(matches!(
        cluster.create(&privileged).await,
        Err(CreateError::Refused { .. })
    ));

    let plain = shell_pod(&dispatcher, &name, "true");
    cluster.admits(&plain).await.unwrap();
    // The dry run created nothing: the name is still free.
    cluster.create(&plain).await.unwrap();
    cluster.delete(&name, PROMPT).await.unwrap();
}
