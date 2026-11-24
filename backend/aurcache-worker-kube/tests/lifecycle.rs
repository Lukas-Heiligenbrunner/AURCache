//! The lifecycle of a build, against a cluster in memory: what the dispatcher
//! asks of the cluster and of the server at each turn a build can take, and
//! what it reports.
//!
//! Time is paused throughout: a wait here ends because something happened or
//! because the clock was moved to where it ends, never because it passed.

mod support;

use aurcache_build_agent::report::{Ended, SetupStep};
use aurcache_common::worker::BuildOutcome;
use aurcache_worker_core::executor::Executor;
use aurcache_worker_core::settings::WorkerSettings;
use aurcache_worker_kube::cluster::{CreateError, Sweep};
use aurcache_worker_kube::pod::{BUILD, HOLD, LABEL_DISPATCHER, STAGE, labels};
use k8s_openapi::api::core::v1::PodStatus;
use std::time::Duration;
use support::{
    AfterLog, Call, DISPATCHER, INPUT, Package, Receive, Rig, Script, Sending, Sink, claimed,
    pod_name, report, status,
};

/// `haystack` contains `needle`, or the test says what it was.
macro_rules! assert_contains {
    ($haystack:expr, $needle:expr) => {{
        let (haystack, needle): (&str, &str) = (&$haystack, &$needle);
        assert!(haystack.contains(needle), "{needle:?} not in:\n{haystack}");
    }};
}

const GRACE: Duration = Duration::from_secs(5);

fn deleted(name: &str) -> Call {
    Call::Delete {
        name: name.to_string(),
        grace: GRACE,
    }
}

/// The pod is deleted exactly once, and is gone.
fn assert_cleaned_up(rig: &Rig, build_id: i32) {
    let name = pod_name(build_id);
    assert_eq!(rig.cluster.deletions(), [deleted(&name)]);
    assert!(!rig.cluster.has_pod(&name));
}

#[tokio::test(start_paused = true)]
async fn a_build_that_succeeds_streams_its_packages() {
    let rig = Rig::new(Script::default());
    let built = rig.build(&claimed(7, None)).await;

    assert_eq!(built.report.outcome, BuildOutcome::Succeeded);
    assert_eq!(built.report.exit_code, Some(0));

    // The pod, then each exchange in its container, in order.
    let name = pod_name(7);
    assert_eq!(rig.cluster.calls()[0], Call::Create(name.clone()));
    assert_eq!(
        rig.cluster.execs(),
        [
            format!("{STAGE}: receive --work /work"),
            format!("{HOLD}: tail --work /work --from 0"),
            format!("{HOLD}: manifest --work /work"),
            format!("{HOLD}: send --work /work --name=hello-1.0-1-x86_64.pkg.tar.zst"),
            format!("{HOLD}: send --work /work --name=hello-docs-1.0-1-any.pkg.tar.zst"),
        ]
    );
    let received = Call::Received(INPUT.to_vec());
    assert!(rig.cluster.calls().contains(&received));

    // Each upload declared the manifest's size and carried the file's bytes.
    let taken = rig.sink.taken();
    assert_eq!(taken.len(), 2);
    assert_eq!(taken[0].build_id, 7);
    assert_eq!(taken[0].name, "hello-1.0-1-x86_64.pkg.tar.zst");
    assert_eq!(taken[0].len, 17);
    assert_eq!(taken[0].bytes, b"the hello package");
    assert_eq!(taken[1].name, "hello-docs-1.0-1-any.pkg.tar.zst");
    assert_eq!(
        taken[1].bytes,
        b"the documentation of hello, which is longer"
    );

    assert_contains!(
        built.log,
        "==> Making package: hello\n==> Finished making: hello\n"
    );
    assert_contains!(built.log, format!("[worker] created pod {name}\n"));
    assert_contains!(built.log, "[worker] the build is running\n");
    assert_contains!(
        built.log,
        "[worker] uploading hello-1.0-1-x86_64.pkg.tar.zst\n"
    );
    assert_cleaned_up(&rig, 7);
}

/// One way a build can end, and what the dispatcher reports of it.
struct EndCase {
    script: Script,
    outcome: BuildOutcome,
    /// A part of the report's reason.
    reason: &'static str,
    /// How many `send`s it took.
    sends: usize,
}

/// How each way a build ends is reported. Only a success hands packages
/// back; whatever happens, the pod is deleted.
#[tokio::test(start_paused = true)]
async fn each_ending_is_reported_as_the_package_or_the_worker() {
    let reporting = |ended: Ended| Script {
        report: Some(report(ended)),
        ..Script::default()
    };
    let manifest = |text: &str| Script {
        manifest: Some(text.to_string()),
        ..Script::default()
    };
    let evicted = |message: &str| Script {
        report: None,
        after_log: AfterLog::Status(Box::new(status::evicted(message))),
        ..Script::default()
    };
    let cases = [
        EndCase {
            script: reporting(Ended::Exited { code: 2 }),
            outcome: BuildOutcome::Failed,
            reason: "build failed (exit 2)",
            sends: 0,
        },
        // A step before makepkg is the worker's, not the package's.
        EndCase {
            script: reporting(Ended::Setup {
                step: SetupStep::Keyring,
                message: "pacman-key --init failed".to_string(),
            }),
            outcome: BuildOutcome::Interrupted,
            reason: "keyring",
            sends: 0,
        },
        EndCase {
            script: Script {
                packages: Vec::new(),
                ..Script::default()
            },
            outcome: BuildOutcome::Failed,
            reason: "build produced no artifacts",
            sends: 0,
        },
        // A name that is not a package's never reaches a URL, or the pod.
        EndCase {
            script: manifest(
                r#"{"interface":1,"report":{"interface":1,"ended":{"how":"exited","code":0}},
                    "log_size":0,"artifacts":[{"name":"../x","size":1}]}"#,
            ),
            outcome: BuildOutcome::Failed,
            reason: "build produced no artifacts",
            sends: 0,
        },
        EndCase {
            script: manifest(r#"{"interface":2,"report":null,"log_size":0,"artifacts":[]}"#),
            outcome: BuildOutcome::Interrupted,
            reason: "interface 2",
            sends: 0,
        },
        // On cgroup v2 an out-of-memory kill takes the whole build container,
        // the agent with it (the kubelet sets memory.oom.group).
        EndCase {
            script: Script::unreported(137, "OOMKilled"),
            outcome: BuildOutcome::Failed,
            reason: "out of memory: the kernel killed the build at the build's memory limit of \
                     4.0 GiB (WORKER_BUILD_MEMORY_MAX)",
            sends: 0,
        },
        EndCase {
            script: evicted("Usage of EmptyDir volume \"work\" exceeds the limit \"30Gi\". "),
            outcome: BuildOutcome::Failed,
            reason: "out of disk: the build passed its ephemeral-storage limit of 30.0 GiB \
                     (WORKER_BUILD_DISK_MAX) and was evicted",
            sends: 0,
        },
        EndCase {
            script: evicted("The node was low on resource: memory. "),
            outcome: BuildOutcome::Interrupted,
            reason: "low on resource",
            sends: 0,
        },
        EndCase {
            script: Script {
                start: status::evicted("The node was low on resource: memory. "),
                ..Script::default()
            },
            outcome: BuildOutcome::Interrupted,
            reason: "evicted from its node",
            sends: 0,
        },
        EndCase {
            script: Script {
                report: None,
                after_log: AfterLog::Gone,
                ..Script::default()
            },
            outcome: BuildOutcome::Interrupted,
            reason: "disappeared",
            sends: 0,
        },
        EndCase {
            script: Script {
                creation: Some(CreateError::Refused {
                    message: "pods \"x\" is forbidden: exceeded quota: builds".to_string(),
                }),
                ..Script::default()
            },
            outcome: BuildOutcome::Interrupted,
            reason: "the cluster refused the build pod: pods \"x\" is forbidden: exceeded quota",
            sends: 0,
        },
        // A hand-in that failed is never tried again: nobody knows whether a
        // build started on it.
        EndCase {
            script: Script {
                receive: Receive::Fails(
                    "exit code 1; stderr: the job was already handed in".to_string(),
                ),
                ..Script::default()
            },
            outcome: BuildOutcome::Interrupted,
            reason: "handing the job to the build pod failed: exit code 1; stderr: the job was \
                     already handed in",
            sends: 0,
        },
        EndCase {
            script: Script {
                packages: vec![Package::of("hello-1.0-1-x86_64.pkg.tar.zst", b"hello")],
                sends: [
                    Sending::Breaks { after: 2 },
                    Sending::Fails("exit code 1; stderr: cannot open".to_string()),
                ]
                .into(),
                ..Script::default()
            },
            outcome: BuildOutcome::Interrupted,
            reason: "again, after a first attempt failed",
            sends: 2,
        },
    ];
    for EndCase {
        script,
        outcome,
        reason,
        sends,
    } in cases
    {
        let rig = Rig::new(script);
        let built = rig.build(&claimed(7, None)).await;
        assert_eq!(built.report.outcome, outcome, "{reason}");
        assert_contains!(built.reason(), reason);
        assert_eq!(rig.cluster.execs_of("send").len(), sends, "{reason}");
        assert!(rig.cluster.execs_of("receive").len() <= 1, "{reason}");
        assert_cleaned_up(&rig, 7);
    }
}

/// A build cancelled while it is in some phase.
struct CancelCase {
    /// Keeps the build in its phase.
    script: Script,
    /// What the log holds by the cancel.
    logged: &'static str,
    /// How many times the job was handed in.
    received: usize,
}

/// A cancel while the pod waits for a node, while it builds, and while it
/// hands back, comes in once the build has settled in that phase. It stops
/// the build, hands nothing back and deletes the pod.
#[tokio::test(start_paused = true)]
async fn a_cancel_in_any_phase_deletes_the_pod() {
    let cases = [
        CancelCase {
            script: Script {
                start: status::unschedulable("0/3 nodes are available"),
                ..Script::default()
            },
            logged: "[worker] no node can take the build pod yet: 0/3 nodes are available\n",
            received: 0,
        },
        CancelCase {
            script: Script {
                log: b"==> Making package: hello\n==> Building".to_vec(),
                after_log: AfterLog::Nothing,
                ..Script::default()
            },
            logged: "==> Building\n",
            received: 1,
        },
        CancelCase {
            script: Script {
                sends: [Sending::Hangs].into(),
                ..Script::default()
            },
            logged: "==> Finished making: hello\n",
            received: 1,
        },
    ];
    for CancelCase {
        script,
        logged,
        received,
    } in cases
    {
        let rig = Rig::new(script);
        let job = claimed(7, None);
        let cancel = job.cancel.clone();
        let (built, ()) = tokio::join!(rig.build(&job), async {
            rig.cluster
                .until(|calls| calls.contains(&Call::Create(pod_name(7))))
                .await;
            tokio::time::sleep(Duration::from_secs(30)).await;
            cancel.cancel();
        });
        assert_eq!(built.reason(), "build canceled", "{logged}");
        assert_contains!(built.log, logged);
        assert_eq!(rig.cluster.execs_of("receive").len(), received, "{logged}");
        assert!(rig.sink.taken().is_empty(), "{logged}");
        assert_cleaned_up(&rig, 7);
    }
}

/// A build that is stopped before it has a pod gets none: a request to
/// create one would only have to be undone.
#[tokio::test(start_paused = true)]
async fn a_build_cancelled_before_its_pod_is_made_makes_none() {
    let rig = Rig::new(Script::default());
    let job = claimed(7, None);
    job.cancel.cancel();
    let built = rig.build(&job).await;
    assert_eq!(built.report.outcome, BuildOutcome::Canceled);
    assert!(
        !rig.cluster.calls().contains(&Call::Create(pod_name(7))),
        "{:?}",
        rig.cluster.calls()
    );
    assert!(!rig.cluster.has_pod(&pod_name(7)));
}

#[tokio::test(start_paused = true)]
async fn a_build_past_its_timeout_is_stopped_and_reported_as_timed_out() {
    let rig = Rig::new(Script {
        after_log: AfterLog::Nothing,
        ..Script::default()
    });
    let started = tokio::time::Instant::now();
    let built = rig.build(&claimed(7, Some(Duration::from_secs(900)))).await;
    assert_eq!(started.elapsed(), Duration::from_secs(900));
    assert_eq!(built.report.outcome, BuildOutcome::Failed);
    assert_eq!(built.report.exit_code, Some(124));
    assert_eq!(
        built.reason(),
        "build timed out after 900s (the package's job timeout, 15m)"
    );
    assert_cleaned_up(&rig, 7);
}

/// The timeout is the build's: packages are still handed back after it.
#[tokio::test(start_paused = true)]
async fn the_timeout_does_not_stop_the_hand_back() {
    let rig = Rig::new(Script {
        sends: [Sending::Slow {
            pause: Duration::from_secs(250),
        }]
        .into(),
        ..Script::default()
    });
    let started = tokio::time::Instant::now();
    let built = rig.build(&claimed(7, Some(Duration::from_secs(200)))).await;
    assert_eq!(started.elapsed(), Duration::from_secs(250));
    assert_eq!(built.report.outcome, BuildOutcome::Succeeded);
    assert_eq!(rig.sink.taken().len(), 2);
}

/// A status that the pod takes `after_s` seconds after its creation.
struct Later {
    after_s: u64,
    status: PodStatus,
}

/// One way a pod's start can go against the start deadline of 10 minutes.
struct StartCase {
    script: Script,
    later: Option<Later>,
    /// For a build given back: what it was last seen doing. `None` for a pod
    /// that starts in time.
    last: Option<&'static str>,
}

/// One deadline, counted from the pod's creation, covers every wait before
/// the build runs. A pod that starts in time builds for as long as it needs;
/// one that does not has its build handed back as the worker's, and not left
/// to run into the package's own timeout.
#[tokio::test(start_paused = true)]
async fn a_pod_must_start_within_the_start_deadline() {
    let starting = |status: PodStatus| Script {
        start: status,
        ..Script::default()
    };
    let unschedulable = || status::unschedulable("0/3 nodes are available");
    let cases = [
        StartCase {
            script: starting(unschedulable()),
            later: None,
            last: Some("no node can take the build pod yet: 0/3 nodes are available"),
        },
        StartCase {
            script: starting(unschedulable()),
            later: Some(Later {
                after_s: 590,
                status: status::staged(),
            }),
            last: None,
        },
        StartCase {
            script: starting(status::starting("ContainerCreating")),
            later: None,
            last: Some("the build pod is starting (ContainerCreating)"),
        },
        // The time spent waiting for a node counts against starting on it.
        StartCase {
            script: starting(unschedulable()),
            later: Some(Later {
                after_s: 400,
                status: status::starting("PodInitializing"),
            }),
            last: Some("the build pod is starting (PodInitializing)"),
        },
        StartCase {
            script: starting(status::stage_image_missing()),
            later: None,
            last: Some("the build pod is starting (ErrImagePull: manifest unknown)"),
        },
        // The node tries a failed pull again by itself.
        StartCase {
            script: starting(status::stage_image_missing()),
            later: Some(Later {
                after_s: 90,
                status: status::staged(),
            }),
            last: None,
        },
        // A builder image that cannot be pulled, once the job is in.
        StartCase {
            script: Script {
                after_receive: status::builder_image_missing(),
                report: None,
                after_log: AfterLog::Nothing,
                log: Vec::new(),
                ..Script::default()
            },
            later: None,
            last: Some("the build container is starting (ImagePullBackOff: Back-off pulling"),
        },
        StartCase {
            script: Script {
                receive: Receive::Hangs,
                ..Script::default()
            },
            later: None,
            last: Some("the job was being handed to the pod"),
        },
    ];
    for StartCase {
        script,
        later,
        last,
    } in cases
    {
        let rig = Rig::new(script);
        let started = tokio::time::Instant::now();
        let job = claimed(7, Some(Duration::from_secs(3 * 3600)));
        let (built, ()) = tokio::join!(rig.build(&job), async {
            if let Some(Later { after_s, status }) = later {
                tokio::time::sleep(Duration::from_secs(after_s)).await;
                rig.cluster.set_status(&pod_name(7), status);
            }
        });
        let Some(last) = last else {
            assert_eq!(built.report.outcome, BuildOutcome::Succeeded);
            continue;
        };
        assert_eq!(started.elapsed(), Duration::from_secs(600), "{last}");
        assert_eq!(built.report.outcome, BuildOutcome::Interrupted, "{last}");
        assert_contains!(
            built.reason(),
            format!(
                "did not start within 10m (WORKER_KUBE_SCHEDULE_TIMEOUT); when last seen, {last}"
            )
        );
        assert_cleaned_up(&rig, 7);
    }
}

/// The kubelet's last lines of the container are read, once, into the log.
#[tokio::test(start_paused = true)]
async fn a_build_container_that_ended_without_a_report_is_an_interruption() {
    let rig = Rig::new(Script {
        last_words: "the agent was killed\n".to_string(),
        ..Script::unreported(137, "Error")
    });
    let built = rig.build(&claimed(7, None)).await;
    assert_eq!(built.report.outcome, BuildOutcome::Interrupted);
    assert_eq!(
        built.reason(),
        "the build container ended without a report: exit code 137, Error"
    );
    assert!(rig.cluster.calls().contains(&Call::LogTail {
        container: BUILD.to_string(),
        lines: 20
    }));
    assert_contains!(
        built.log,
        "[worker] the build container ended without a report (exit code 137, Error); the last \
         it wrote:\nthe agent was killed\n"
    );
    assert_contains!(built.log, "==> Finished making: hello\n");
    assert!(rig.sink.taken().is_empty());
    assert_cleaned_up(&rig, 7);
}

/// The log is taken up from the last byte received, wherever the stream
/// broke: in the middle of a line, and in the middle of a character.
#[tokio::test(start_paused = true)]
async fn a_log_stream_lost_twice_is_resumed_without_a_repeated_or_missing_byte() {
    let log = "premi\u{e8}re ligne\ndeuxi\u{e8}me ligne\ntroisi\u{e8}me, sans fin de ligne";
    let rig = Rig::new(Script {
        log: log.as_bytes().to_vec(),
        // The first break falls inside the two bytes of the first accent.
        tails: [Sending::Breaks { after: 6 }, Sending::Breaks { after: 20 }].into(),
        chunk: 5,
        ..Script::default()
    });
    let built = rig.build(&claimed(7, None)).await;
    assert_eq!(built.report.outcome, BuildOutcome::Succeeded);
    assert_eq!(
        rig.cluster.execs_of("tail"),
        [
            format!("{HOLD}: tail --work /work --from 0"),
            format!("{HOLD}: tail --work /work --from 6"),
            format!("{HOLD}: tail --work /work --from 26"),
        ]
    );
    assert_eq!(built.forwarded_log(), log);
}

/// A `tail` cut short can still end in success, with nothing at all sent.
/// After the build, the log is followed until it has the length that the
/// manifest gives.
#[tokio::test(start_paused = true)]
async fn the_log_is_read_to_the_manifests_length_whatever_a_tail_says() {
    let log = "first line\nsecond line";
    let rig = Rig::new(Script {
        log: log.as_bytes().to_vec(),
        // The build is over before the log was read at all.
        after_receive: status::built(0, "Completed"),
        tails: [Sending::Cut { after: 0 }, Sending::Cut { after: 11 }].into(),
        ..Script::default()
    });
    let built = rig.build(&claimed(7, None)).await;
    assert_eq!(built.report.outcome, BuildOutcome::Succeeded);
    assert_eq!(
        rig.cluster.execs_of("tail"),
        [
            format!("{HOLD}: tail --work /work --from 0"),
            format!("{HOLD}: tail --work /work --from 0"),
            format!("{HOLD}: tail --work /work --from 11"),
        ]
    );
    assert_eq!(built.forwarded_log(), log);
}

/// A manifest with no log length: the log is read no further, and says so.
#[tokio::test(start_paused = true)]
async fn a_log_without_a_length_is_read_no_further() {
    let rig = Rig::new(Script {
        manifest: Some(
            r#"{"interface":1,"report":{"interface":1,"ended":{"how":"exited","code":2}},
                "log_size":null,"artifacts":[]}"#
                .to_string(),
        ),
        ..Script::default()
    });
    let started = tokio::time::Instant::now();
    let built = rig.build(&claimed(7, None)).await;
    assert!(started.elapsed() < Duration::from_secs(60));
    assert_contains!(
        built.log,
        "[worker] the build log could not be read to its end\n"
    );
}

/// A send that passes nothing for five minutes is given up, and so is the
/// one that tries again.
#[tokio::test(start_paused = true)]
async fn a_stalled_send_is_given_up() {
    let rig = Rig::new(Script {
        packages: vec![Package::of("hello-1.0-1-x86_64.pkg.tar.zst", b"hello")],
        sends: [Sending::Hangs, Sending::Hangs].into(),
        ..Script::default()
    });
    let started = tokio::time::Instant::now();
    let built = rig.build(&claimed(7, None)).await;
    assert_eq!(started.elapsed(), Duration::from_secs(600));
    assert_eq!(built.report.outcome, BuildOutcome::Interrupted);
    assert_contains!(built.reason(), "made no progress for 5 minutes");
    assert_cleaned_up(&rig, 7);
}

/// A stream that breaks under an upload, or ends short with a success,
/// makes the body short, which fails the upload; the file is then sent
/// again from its first byte.
#[tokio::test(start_paused = true)]
async fn a_stream_cut_mid_upload_is_retried_from_the_start() {
    for first in [Sending::Breaks { after: 11 }, Sending::Cut { after: 11 }] {
        let rig = Rig::new(Script {
            packages: vec![Package::of(
                "hello-1.0-1-x86_64.pkg.tar.zst",
                b"hello, in more than one chunk",
            )],
            sends: [first.clone()].into(),
            ..Script::default()
        });
        let built = rig.build(&claimed(7, None)).await;
        assert_eq!(built.report.outcome, BuildOutcome::Succeeded, "{first:?}");
        let taken = rig.sink.taken();
        assert_eq!(taken.len(), 2, "{first:?}");
        assert_eq!(taken[0].bytes, b"hello, in m");
        assert_eq!(taken[1].bytes, b"hello, in more than one chunk");
        assert_eq!(taken[1].len, 29);
    }
}

/// A file that the server refuses for what it is fails the build, and is
/// not sent again.
#[tokio::test(start_paused = true)]
async fn a_package_the_server_refuses_is_not_retried() {
    let mut rig = Rig::new(Script::default());
    let refusal = "413 Payload Too Large: over max_artifact_size";
    rig.sink = Sink::refusing(refusal.to_string());
    let built = rig.build(&claimed(7, None)).await;
    assert_eq!(built.report.outcome, BuildOutcome::Failed);
    assert_contains!(built.reason(), "max_artifact_size");
    assert_contains!(built.log, format!("\n[worker] {}\n", built.reason()));
    assert_eq!(rig.cluster.execs_of("send").len(), 1);
    assert!(rig.sink.taken().is_empty());
    assert_cleaned_up(&rig, 7);
}

/// The startup sweep deletes every build pod, this dispatcher's and those of
/// one whose identity is gone, since a builds namespace is one dispatcher's,
/// and leaves a pod that no dispatcher made.
#[tokio::test(start_paused = true)]
async fn the_startup_sweep_takes_any_dispatchers_pods() {
    let rig = Rig::new(Script::default());
    let left = |name: &str, labels| {
        rig.cluster
            .leave(name, labels, status::building(), Duration::ZERO);
    };
    left(&pod_name(3), labels(DISPATCHER));
    let lost = "aurcache-build-fedcba9876543210-4";
    let other = "fedcba9876543210";
    left(lost, labels(other));
    left(
        "unrelated",
        [(LABEL_DISPATCHER.to_string(), other.to_string())].into(),
    );
    assert_eq!(rig.executor.sweep().await.unwrap(), 2);
    assert_eq!(rig.cluster.calls(), [Call::Sweep(Sweep::Namespace)]);
    assert!(!rig.cluster.has_pod(&pod_name(3)));
    assert!(!rig.cluster.has_pod(lost));
    assert!(rig.cluster.has_pod("unrelated"));
    assert_eq!(rig.executor.sweep().await.unwrap(), 0);
}

/// A delete that fails is tried once more.
#[tokio::test(start_paused = true)]
async fn a_failed_delete_is_tried_again() {
    let rig = Rig::new(Script {
        failing_deletes: 1,
        ..Script::default()
    });
    let built = rig.build(&claimed(7, None)).await;
    assert_eq!(built.report.outcome, BuildOutcome::Succeeded);
    assert_eq!(
        rig.cluster.deletions(),
        [deleted(&pod_name(7)), deleted(&pod_name(7))]
    );
    assert!(!rig.cluster.has_pod(&pod_name(7)));
}

/// A pod with no node holds claims back until it starts, or until it is
/// older than the start timeout and no build waits on it.
#[tokio::test(start_paused = true)]
async fn no_claim_while_a_pod_is_unschedulable() {
    let rig = Rig::new(Script::default());
    assert!(rig.executor.ready_for_work(0).await);
    let unschedulable = || status::unschedulable("0/3 nodes are available");
    let left = |name: &str, age_s| {
        let age = Duration::from_secs(age_s);
        rig.cluster
            .leave(name, labels(DISPATCHER), unschedulable(), age);
    };
    left(&pod_name(3), 60);
    assert!(!rig.executor.ready_for_work(1).await);
    rig.cluster.set_status(&pod_name(3), status::building());
    assert!(rig.executor.ready_for_work(1).await);
    left(&pod_name(4), 601);
    assert!(rig.executor.ready_for_work(1).await);
}

/// The startup dry run asks about a pod that no build has, creates nothing,
/// and fails with the cluster's word.
#[tokio::test(start_paused = true)]
async fn the_startup_dry_run_fails_with_the_clusters_refusal() {
    let rig = Rig::new(Script::default());
    rig.executor.check_admission().await.unwrap();
    assert_eq!(rig.cluster.calls(), [Call::Admits(pod_name(0))]);
    assert!(!rig.cluster.has_pod(&pod_name(0)));

    let rig = Rig::new(Script {
        refusal: Some("violates PodSecurity \"baseline:latest\"".to_string()),
        ..Script::default()
    });
    let refused = format!("{:#}", rig.executor.check_admission().await.unwrap_err());
    assert_contains!(refused, "violates PodSecurity");
}

/// Delivered settings are taken for the next build's pod.
#[tokio::test(start_paused = true)]
async fn new_settings_apply_to_the_next_build() {
    use aurcache_common::worker_config::ConfigSnapshot;
    let rig = Rig::new(Script::default());
    let declared = WorkerSettings::from_env(aurcache_worker_core::settings::protocol_settings())
        .extended(aurcache_worker_kube::settings::kube_settings());
    let delivered = declared.with_snapshot(&ConfigSnapshot {
        revision: "1".to_string(),
        settings: [("build_memory_max".to_string(), "8G".to_string())].into(),
    });
    let taken = rig.executor.reconfigure(delivered.clone(), &declared).await;
    assert_eq!(taken.effective(), delivered.effective());

    rig.build(&claimed(7, None)).await;
    let rendered = rig.cluster.rendered();
    let build = &rendered[0].spec.as_ref().unwrap().containers[0];
    let limits = build.resources.as_ref().unwrap().limits.as_ref().unwrap();
    assert_eq!(limits["memory"].0, (8_u64 << 30).to_string());
    assert!(!limits.contains_key("ephemeral-storage"));
}

/// With a key, the dispatcher execs `ssh-link` into the build container
/// once, at the link's socket, and the build goes on when the link fails.
/// What passes over a link is `link::serve`'s to test.
#[tokio::test(start_paused = true)]
async fn the_ssh_link_is_opened_once() {
    let mut rig = Rig::new(Script::default());
    rig.ssh = Some("/nonexistent/aurcache-ssh-agent/agent.sock".into());
    let built = rig.build(&claimed(7, None)).await;
    assert_eq!(built.report.outcome, BuildOutcome::Succeeded);
    assert_eq!(
        rig.cluster.execs_of("ssh-link"),
        ["build: ssh-link --socket /work/scratch/ssh-link/link.sock"]
    );
    assert_cleaned_up(&rig, 7);
}
