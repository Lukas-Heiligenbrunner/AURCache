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
use aurcache_worker_kube::cluster::{Admission, CreateError, Sweep};
use aurcache_worker_kube::config::Config;
use aurcache_worker_kube::executor;
use aurcache_worker_kube::pod::{BUILD, HOLD, STAGE};
use std::sync::Arc;
use std::time::Duration;
use support::{
    AfterLog, Answer, Call, HostUsers, INPUT, Package, Receive, Rig, Script, Sending, Sink,
    SshLinkScript, Start, VCS_COMMIT, VCS_SOURCE, claimed, config, input, pod_name, report, status,
};

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

fn reason(built: &support::Built) -> &str {
    built.report.reason.as_deref().unwrap_or_default()
}

#[tokio::test(start_paused = true)]
async fn a_build_that_succeeds_streams_its_packages_and_reports_what_it_measured() {
    let rig = Rig::new(Script::default());
    let built = rig.build(&claimed(7, None)).await;

    assert_eq!(built.report.outcome, BuildOutcome::Succeeded);
    assert_eq!(built.report.exit_code, Some(0));
    assert_eq!(built.report.peak_memory_bytes, Some(3 << 30));
    assert_eq!(built.report.disk_usage.unwrap().workdir, Some(2 << 30));
    assert_eq!(built.report.vcs_commits[VCS_SOURCE], VCS_COMMIT);

    // The pod, then each exchange in its container, in order.
    let name = pod_name(7);
    assert_eq!(rig.cluster.calls()[0], Call::Create(name.clone()));
    assert_eq!(
        rig.cluster.execs(),
        [
            format!("{STAGE}: receive --work /work"),
            format!("{HOLD}: tail --work /work --from 0"),
            // The log ends with a tail that has nothing more to send.
            format!("{HOLD}: tail --work /work --from 53"),
            format!("{HOLD}: manifest --work /work"),
            format!("{HOLD}: send --work /work --name hello-1.0-1-x86_64.pkg.tar.zst"),
            format!("{HOLD}: send --work /work --name hello-docs-1.0-1-any.pkg.tar.zst"),
        ]
    );
    assert_eq!(rig.cluster.received(), [INPUT.to_vec()]);

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

    assert!(
        built
            .log
            .contains("==> Making package: hello\n==> Finished making: hello\n"),
        "{}",
        built.log
    );
    assert!(
        built
            .log
            .contains(&format!("[worker] created pod {name}\n"))
    );
    assert!(built.log.contains("[worker] the build is running\n"));
    assert!(
        built
            .log
            .contains("[worker] uploading hello-1.0-1-x86_64.pkg.tar.zst\n")
    );
    assert_cleaned_up(&rig, 7);
}

/// The pod carries the build's own deadline and the limits in force.
#[tokio::test(start_paused = true)]
async fn the_pod_is_rendered_from_the_job_and_the_configuration() {
    let rig = Rig::new(Script::default());
    rig.build(&claimed(7, Some(Duration::from_secs(3600))))
        .await;
    let rendered = rig.cluster.rendered();
    let spec = rendered[0].spec.as_ref().unwrap();
    assert_eq!(spec.active_deadline_seconds, Some(3600 + 300));
    assert_eq!(spec.init_containers.as_ref().unwrap()[0].name, STAGE);
    let build = &spec.containers[0];
    assert_eq!(build.name, BUILD);
    let limits = build.resources.as_ref().unwrap().limits.as_ref().unwrap();
    assert_eq!(limits["memory"].0, (4_u64 << 30).to_string());
}

/// A build that failed is reported from its report, and hands nothing back.
#[tokio::test(start_paused = true)]
async fn a_failed_build_uploads_nothing_and_keeps_its_measurements() {
    let rig = Rig::new(Script {
        report: Some(report(Ended::Exited { code: 2 })),
        ..Script::default()
    });
    let built = rig.build(&claimed(7, None)).await;
    assert_eq!(built.report.outcome, BuildOutcome::Failed);
    assert_eq!(built.report.exit_code, Some(2));
    assert_eq!(built.report.peak_memory_bytes, Some(3 << 30));
    assert!(rig.sink.taken().is_empty());
    assert!(!rig.cluster.execs().iter().any(|exec| exec.contains("send")));
    assert_cleaned_up(&rig, 7);
}

/// A step before makepkg that failed is the worker's, not the package's.
#[tokio::test(start_paused = true)]
async fn a_failed_setup_step_is_an_interruption() {
    let rig = Rig::new(Script {
        report: Some(report(Ended::Setup {
            step: SetupStep::Keyring,
            message: "pacman-key --init failed".to_string(),
        })),
        ..Script::default()
    });
    let built = rig.build(&claimed(7, None)).await;
    assert_eq!(built.report.outcome, BuildOutcome::Interrupted);
    assert!(reason(&built).contains("keyring"), "{}", reason(&built));
    assert!(rig.sink.taken().is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_build_that_made_no_package_fails() {
    let rig = Rig::new(Script {
        packages: Vec::new(),
        ..Script::default()
    });
    let built = rig.build(&claimed(7, None)).await;
    assert_eq!(built.report.outcome, BuildOutcome::Failed);
    assert_eq!(reason(&built), "build produced no artifacts");
    assert!(built.log.contains("[worker] build produced no artifacts"));
    assert_eq!(built.report.peak_memory_bytes, Some(3 << 30));
}

#[tokio::test(start_paused = true)]
async fn a_cancel_while_the_pod_waits_for_a_node_deletes_it() {
    let rig = Rig::new(Script {
        start: Start::Status(Box::new(status::unschedulable("0/3 nodes are available"))),
        ..Script::default()
    });
    let job = claimed(7, None);
    let cancel = job.cancel.clone();
    let (built, ()) = tokio::join!(rig.build(&job), async {
        rig.cluster
            .until(|calls| calls.contains(&Call::Create(pod_name(7))))
            .await;
        tokio::time::sleep(Duration::from_secs(30)).await;
        cancel.cancel();
    });
    assert_eq!(built.report.outcome, BuildOutcome::Canceled);
    assert!(rig.cluster.execs().is_empty(), "nothing was handed in");
    assert!(
        built
            .log
            .contains("[worker] no node can take the build pod yet: 0/3 nodes are available\n"),
        "{}",
        built.log
    );
    assert_cleaned_up(&rig, 7);
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
async fn a_cancel_while_building_deletes_the_pod() {
    let rig = Rig::new(Script {
        after_log: AfterLog::Nothing,
        ..Script::default()
    });
    let job = claimed(7, Some(Duration::from_secs(3600)));
    let cancel = job.cancel.clone();
    let (built, ()) = tokio::join!(rig.build(&job), async {
        rig.cluster.until_exec("tail").await;
        tokio::time::sleep(Duration::from_secs(60)).await;
        cancel.cancel();
    });
    assert_eq!(built.report.outcome, BuildOutcome::Canceled);
    assert_eq!(reason(&built), "build canceled");
    // What the build had written reached the log before the stop.
    assert!(built.log.contains("==> Finished making: hello\n"));
    assert!(rig.sink.taken().is_empty());
    assert_cleaned_up(&rig, 7);
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
        reason(&built),
        "build timed out after 900s (the package's job timeout, 15m)"
    );
    assert_cleaned_up(&rig, 7);
}

/// The timeout is the build's: packages are still handed back after it.
#[tokio::test(start_paused = true)]
async fn the_timeout_does_not_stop_the_hand_back() {
    let rig = Rig::new(Script {
        sends: [Sending::Slow {
            pause: Duration::from_secs(1000),
        }]
        .into(),
        ..Script::default()
    });
    let started = tokio::time::Instant::now();
    let built = rig.build(&claimed(7, Some(Duration::from_secs(900)))).await;
    assert_eq!(started.elapsed(), Duration::from_secs(1000));
    assert_eq!(built.report.outcome, BuildOutcome::Succeeded);
    assert_eq!(rig.sink.taken().len(), 2);
}

/// A cancel does: the server no longer wants what the build made.
#[tokio::test(start_paused = true)]
async fn a_cancel_during_the_hand_back_stops_it() {
    let rig = Rig::new(Script {
        packages: vec![Package::of("hello-1.0-1-x86_64.pkg.tar.zst", b"hello")],
        sends: [Sending::Hangs].into(),
        ..Script::default()
    });
    let job = claimed(7, None);
    let cancel = job.cancel.clone();
    let (built, ()) = tokio::join!(rig.build(&job), async {
        rig.cluster.until_exec("send").await;
        cancel.cancel();
    });
    assert_eq!(built.report.outcome, BuildOutcome::Canceled);
    assert_cleaned_up(&rig, 7);
}

#[tokio::test(start_paused = true)]
async fn a_pod_unschedulable_past_the_timeout_gives_the_build_back() {
    let rig = Rig::new(Script {
        start: Start::Status(Box::new(status::unschedulable(
            "0/3 nodes are available: 3 Insufficient memory.",
        ))),
        ..Script::default()
    });
    let started = tokio::time::Instant::now();
    let built = rig.build(&claimed(7, None)).await;
    assert_eq!(started.elapsed(), Duration::from_secs(600));
    assert_eq!(built.report.outcome, BuildOutcome::Interrupted);
    assert_eq!(
        reason(&built),
        "no node could take the build within 10m (WORKER_KUBE_SCHEDULE_TIMEOUT): 0/3 nodes are \
         available: 3 Insufficient memory."
    );
    assert!(rig.cluster.execs().is_empty());
    assert_cleaned_up(&rig, 7);
}

/// A pod that found a node in time is not held to the schedule timeout
/// afterwards.
#[tokio::test(start_paused = true)]
async fn a_pod_scheduled_late_but_in_time_builds() {
    let rig = Rig::new(Script {
        start: Start::Status(Box::new(status::unschedulable("0/3 nodes are available"))),
        ..Script::default()
    });
    let job = claimed(7, None);
    let (built, ()) = tokio::join!(rig.build(&job), async {
        tokio::time::sleep(Duration::from_secs(590)).await;
        rig.cluster.set_status(&pod_name(7), status::staged());
    });
    assert_eq!(built.report.outcome, BuildOutcome::Succeeded);
}

/// A pod a node took and that never starts, as one whose Secret does not
/// exist or whose network cannot be set up: the build is handed back as the
/// worker's, and not left to run into the package's own timeout.
#[tokio::test(start_paused = true)]
async fn a_pod_that_does_not_start_gives_the_build_back() {
    let rig = Rig::new(Script {
        start: Start::Status(Box::new(status::starting("ContainerCreating"))),
        ..Script::default()
    });
    let started = tokio::time::Instant::now();
    let built = rig
        .build(&claimed(7, Some(Duration::from_secs(3 * 3600))))
        .await;
    assert_eq!(started.elapsed(), Duration::from_secs(600));
    assert_eq!(built.report.outcome, BuildOutcome::Interrupted);
    assert_eq!(
        reason(&built),
        "the build pod did not start within 10m of getting a node \
         (WORKER_KUBE_SCHEDULE_TIMEOUT); when last seen, the build pod is starting \
         (ContainerCreating). `kubectl -n aurcache-builds get events` says what holds it: a \
         volume that cannot be mounted (a Secret that does not exist), or a network the node \
         cannot give the pod"
    );
    assert!(rig.cluster.execs().is_empty());
    assert_cleaned_up(&rig, 7);
}

/// The wait for a node does not count against the time to start on it.
#[tokio::test(start_paused = true)]
async fn a_pod_gets_as_long_to_start_as_it_had_to_find_a_node() {
    let rig = Rig::new(Script {
        start: Start::Status(Box::new(status::unschedulable("0/3 nodes are available"))),
        ..Script::default()
    });
    let job = claimed(7, None);
    let (built, ()) = tokio::join!(rig.build(&job), async {
        tokio::time::sleep(Duration::from_secs(590)).await;
        rig.cluster
            .set_status(&pod_name(7), status::starting("PodInitializing"));
        tokio::time::sleep(Duration::from_secs(590)).await;
        rig.cluster.set_status(&pod_name(7), status::staged());
    });
    assert_eq!(built.report.outcome, BuildOutcome::Succeeded);
}

/// The node tries a failed pull again by itself, and a registry that
/// answers on a later try has cost the build nothing.
#[tokio::test(start_paused = true)]
async fn an_image_pulled_on_a_later_try_costs_nothing() {
    let rig = Rig::new(Script {
        start: Start::Status(Box::new(status::stage_image_missing())),
        ..Script::default()
    });
    let job = claimed(7, None);
    let (built, ()) = tokio::join!(rig.build(&job), async {
        tokio::time::sleep(Duration::from_secs(90)).await;
        rig.cluster.set_status(&pod_name(7), status::staged());
    });
    assert_eq!(built.report.outcome, BuildOutcome::Succeeded);
    assert!(
        built.log.contains(
            "[worker] the stage container cannot start: ErrImagePull: manifest unknown; the \
             node tries again\n"
        ),
        "{}",
        built.log
    );
}

#[tokio::test(start_paused = true)]
async fn an_agent_image_that_cannot_be_pulled_is_an_interruption() {
    let rig = Rig::new(Script {
        start: Start::Status(Box::new(status::stage_image_missing())),
        ..Script::default()
    });
    let started = tokio::time::Instant::now();
    let built = rig.build(&claimed(7, None)).await;
    assert_eq!(started.elapsed(), Duration::from_secs(120));
    assert_eq!(built.report.outcome, BuildOutcome::Interrupted);
    assert_eq!(
        reason(&built),
        "the stage container cannot start: ErrImagePull: manifest unknown"
    );
    assert_cleaned_up(&rig, 7);
}

#[tokio::test(start_paused = true)]
async fn a_builder_image_that_cannot_be_pulled_is_an_interruption() {
    let rig = Rig::new(Script {
        report: None,
        after_log: AfterLog::Status(Box::new(status::builder_image_missing())),
        log: Vec::new(),
        ..Script::default()
    });
    let built = rig.build(&claimed(7, None)).await;
    assert_eq!(built.report.outcome, BuildOutcome::Interrupted);
    assert_eq!(
        reason(&built),
        "the build container cannot start: ImagePullBackOff: Back-off pulling image"
    );
    assert_cleaned_up(&rig, 7);
}

#[tokio::test(start_paused = true)]
async fn a_build_killed_for_memory_with_no_report_fails_naming_the_limit() {
    let rig = Rig::new(Script {
        last_words: "cc1plus: out of memory\nKilled\n".to_string(),
        ..Script::unreported(137, "OOMKilled")
    });
    let built = rig.build(&claimed(7, None)).await;
    assert_eq!(built.report.outcome, BuildOutcome::Failed);
    assert_eq!(built.report.exit_code, Some(137));
    assert_eq!(
        reason(&built),
        "out of memory: the kernel killed the build at the build's memory limit of 4.0 GiB \
         (WORKER_BUILD_MEMORY_MAX)"
    );
    // The kubelet's last lines of the container are read, once, into the log.
    assert!(rig.cluster.calls().contains(&Call::LogTail {
        container: BUILD.to_string(),
        lines: 20
    }));
    assert!(
        built.log.contains(
            "[worker] the build container ended without a report (exit code 137, OOMKilled); \
             the last it wrote:\ncc1plus: out of memory\nKilled\n"
        ),
        "{}",
        built.log
    );
    assert!(built.log.contains("==> Finished making: hello\n"));
    assert_cleaned_up(&rig, 7);
}

#[tokio::test(start_paused = true)]
async fn a_build_container_that_ended_without_a_report_is_an_interruption() {
    let rig = Rig::new(Script::unreported(101, "Error"));
    let built = rig.build(&claimed(7, None)).await;
    assert_eq!(built.report.outcome, BuildOutcome::Interrupted);
    assert_eq!(
        reason(&built),
        "the build container ended without a report: exit code 101, Error"
    );
    assert!(rig.sink.taken().is_empty());
    assert_cleaned_up(&rig, 7);
}

#[tokio::test(start_paused = true)]
async fn an_eviction_for_the_builds_own_limit_fails_it() {
    let rig = Rig::new(Script {
        report: None,
        after_log: AfterLog::Status(Box::new(status::evicted(
            "Usage of EmptyDir volume \"work\" exceeds the limit \"30Gi\". ",
        ))),
        ..Script::default()
    });
    let built = rig.build(&claimed(7, None)).await;
    assert_eq!(built.report.outcome, BuildOutcome::Failed);
    assert_eq!(
        reason(&built),
        "out of disk: the build passed its ephemeral-storage limit of 30.0 GiB \
         (WORKER_BUILD_DISK_MAX) and was evicted"
    );
    assert!(
        !rig.cluster
            .execs()
            .iter()
            .any(|exec| exec.contains("manifest"))
    );
    assert_cleaned_up(&rig, 7);
}

#[tokio::test(start_paused = true)]
async fn an_eviction_for_the_nodes_sake_is_an_interruption() {
    let rig = Rig::new(Script {
        report: None,
        after_log: AfterLog::Status(Box::new(status::evicted(
            "The node was low on resource: memory. ",
        ))),
        ..Script::default()
    });
    let built = rig.build(&claimed(7, None)).await;
    assert_eq!(built.report.outcome, BuildOutcome::Interrupted);
    assert!(reason(&built).contains("low on resource"));
    assert_cleaned_up(&rig, 7);
}

#[tokio::test(start_paused = true)]
async fn a_pod_that_vanishes_is_an_interruption() {
    let rig = Rig::new(Script {
        report: None,
        after_log: AfterLog::Gone,
        ..Script::default()
    });
    let built = rig.build(&claimed(7, None)).await;
    assert_eq!(built.report.outcome, BuildOutcome::Interrupted);
    assert!(reason(&built).contains("disappeared"), "{}", reason(&built));
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
        tail_breaks: [6, 20].into(),
        chunk: 5,
        ..Script::default()
    });
    let built = rig.build(&claimed(7, None)).await;
    assert_eq!(built.report.outcome, BuildOutcome::Succeeded);
    let tails: Vec<String> = rig
        .cluster
        .execs()
        .into_iter()
        .filter(|exec| exec.contains("tail"))
        .collect();
    assert_eq!(
        tails,
        [
            format!("{HOLD}: tail --work /work --from 0"),
            format!("{HOLD}: tail --work /work --from 6"),
            format!("{HOLD}: tail --work /work --from 26"),
            // The one that finds nothing more to send.
            format!("{HOLD}: tail --work /work --from 61"),
        ]
    );
    let forwarded: String = built
        .log
        .lines()
        .filter(|line| !line.starts_with("[worker]"))
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(forwarded, log);
}

/// A `tail` cut short can still end in success. The log is followed on from
/// where it stopped, and only a `tail` with nothing more to send ends it.
#[tokio::test(start_paused = true)]
async fn a_log_stream_cut_short_in_success_is_followed_to_its_end() {
    let log = "first line\nsecond line";
    let rig = Rig::new(Script {
        log: log.as_bytes().to_vec(),
        tail_cuts: [11].into(),
        ..Script::default()
    });
    let built = rig.build(&claimed(7, None)).await;
    assert_eq!(built.report.outcome, BuildOutcome::Succeeded);
    let tails: Vec<String> = rig
        .cluster
        .execs()
        .into_iter()
        .filter(|exec| exec.contains("tail"))
        .collect();
    assert_eq!(
        tails,
        [
            format!("{HOLD}: tail --work /work --from 0"),
            format!("{HOLD}: tail --work /work --from 11"),
            format!("{HOLD}: tail --work /work --from 22"),
        ]
    );
    let forwarded: Vec<&str> = built
        .log
        .lines()
        .filter(|line| !line.starts_with("[worker]"))
        .collect();
    assert_eq!(forwarded, ["first line", "second line"]);
}

/// A stream that breaks under an upload makes the body short, which fails
/// the upload; the file is then sent again from its first byte.
#[tokio::test(start_paused = true)]
async fn a_stream_broken_mid_upload_is_retried_from_the_start() {
    let rig = Rig::new(Script {
        packages: vec![Package::of(
            "hello-1.0-1-x86_64.pkg.tar.zst",
            b"hello, in more than one chunk",
        )],
        sends: [Sending::Breaks { after: 11 }].into(),
        ..Script::default()
    });
    let built = rig.build(&claimed(7, None)).await;
    assert_eq!(built.report.outcome, BuildOutcome::Succeeded);
    let taken = rig.sink.taken();
    assert_eq!(taken.len(), 2);
    assert_eq!(taken[0].bytes, b"hello, in m");
    assert_eq!(taken[1].bytes, b"hello, in more than one chunk");
    assert_eq!(taken[1].len, 29);
}

#[tokio::test(start_paused = true)]
async fn a_send_that_fails_twice_is_an_interruption() {
    let rig = Rig::new(Script {
        packages: vec![Package::of("hello-1.0-1-x86_64.pkg.tar.zst", b"hello")],
        sends: [
            Sending::Breaks { after: 2 },
            Sending::Fails("exit code 1; stderr: cannot open".to_string()),
        ]
        .into(),
        ..Script::default()
    });
    let built = rig.build(&claimed(7, None)).await;
    assert_eq!(built.report.outcome, BuildOutcome::Interrupted);
    assert_cleaned_up(&rig, 7);
}

/// A file the server refuses for what it is fails the build, and is not
/// sent again.
#[tokio::test(start_paused = true)]
async fn a_package_the_server_refuses_is_not_retried() {
    let sink = Sink::answering([Answer::Refuses(
        "413 Payload Too Large: over max_artifact_size".to_string(),
    )]);
    let rig = Rig::with(Script::default(), sink);
    let built = rig.build(&claimed(7, None)).await;
    assert_eq!(built.report.outcome, BuildOutcome::Failed);
    assert!(
        reason(&built).contains("max_artifact_size"),
        "{}",
        reason(&built)
    );
    assert_eq!(
        rig.cluster
            .execs()
            .iter()
            .filter(|exec| exec.contains("send"))
            .count(),
        1
    );
    assert!(rig.sink.taken().is_empty());
    assert_cleaned_up(&rig, 7);
}

/// A name that is not a package's never reaches a URL, or the pod.
#[tokio::test(start_paused = true)]
async fn a_manifest_naming_something_that_is_not_a_package_is_refused() {
    let manifest = r#"{"interface":1,"report":{"interface":1,"ended":{"how":"exited","code":0},
        "peak_memory_bytes":null,"oom_kills":null,"disk_bytes":null,"vcs_commits":{}},
        "artifacts":[{"name":"../x","size":1}]}"#;
    let rig = Rig::new(Script {
        manifest: Some(manifest.to_string()),
        ..Script::default()
    });
    let built = rig.build(&claimed(7, None)).await;
    assert_eq!(built.report.outcome, BuildOutcome::Interrupted);
    assert!(reason(&built).contains("\"../x\""), "{}", reason(&built));
    assert!(rig.sink.taken().is_empty());
    assert!(!rig.cluster.execs().iter().any(|exec| exec.contains("send")));
    assert_cleaned_up(&rig, 7);
}

#[tokio::test(start_paused = true)]
async fn a_manifest_from_another_interface_is_refused() {
    let rig = Rig::new(Script {
        manifest: Some(r#"{"interface":2,"report":null,"artifacts":[]}"#.to_string()),
        ..Script::default()
    });
    let built = rig.build(&claimed(7, None)).await;
    assert_eq!(built.report.outcome, BuildOutcome::Interrupted);
    assert!(reason(&built).contains("interface 2"), "{}", reason(&built));
}

#[tokio::test(start_paused = true)]
async fn a_pod_the_cluster_refuses_is_an_interruption() {
    let rig = Rig::new(Script {
        creations: [CreateError::Refused {
            message: "pods \"x\" is forbidden: exceeded quota: builds".to_string(),
        }]
        .into(),
        ..Script::default()
    });
    let built = rig.build(&claimed(7, None)).await;
    assert_eq!(built.report.outcome, BuildOutcome::Interrupted);
    assert_eq!(
        reason(&built),
        "the cluster refused the build pod: pods \"x\" is forbidden: exceeded quota: builds"
    );
    assert!(rig.cluster.execs().is_empty());
}

/// A pod of the build's name, left by an earlier attempt, is replaced.
#[tokio::test(start_paused = true)]
async fn a_leftover_pod_of_the_same_name_is_replaced() {
    let rig = Rig::new(Script::default());
    let name = pod_name(7);
    rig.cluster.leave(&name, status::built(1, "Error"));
    let built = rig.build(&claimed(7, None)).await;
    assert_eq!(built.report.outcome, BuildOutcome::Succeeded);
    let calls = rig.cluster.calls();
    assert_eq!(
        calls[..3],
        [
            Call::Create(name.clone()),
            Call::Delete {
                name: name.clone(),
                grace: aurcache_worker_kube::cluster::PROMPT
            },
            Call::Create(name.clone()),
        ]
    );
    assert!(!rig.cluster.has_pod(&name));
}

/// A hand-in that failed is never tried again: nobody knows whether a build
/// started on it.
#[tokio::test(start_paused = true)]
async fn a_failed_hand_in_is_an_interruption_and_is_not_retried() {
    let rig = Rig::new(Script {
        receive: Receive::Fails("exit code 1; stderr: the job was already handed in".to_string()),
        ..Script::default()
    });
    let built = rig.build(&claimed(7, None)).await;
    assert_eq!(built.report.outcome, BuildOutcome::Interrupted);
    assert_eq!(
        reason(&built),
        "handing the job to the build pod failed: exit code 1; stderr: the job was already \
         handed in"
    );
    assert_eq!(
        rig.cluster.execs(),
        [format!("{STAGE}: receive --work /work")]
    );
    assert_cleaned_up(&rig, 7);
}

/// The pod goes whatever becomes of the task that built in it.
#[tokio::test(start_paused = true)]
async fn a_panic_while_building_still_deletes_the_pod() {
    use aurcache_worker_kube::executor::Build;
    use aurcache_worker_kube::transfer::Uploads;
    use futures::FutureExt;

    struct Panics;
    impl Uploads for Panics {
        async fn upload<R>(&self, _: i32, _: &str, _: R, _: u64) -> anyhow::Result<()>
        where
            R: tokio::io::AsyncRead + Send + Unpin + 'static,
        {
            panic!("the upload panicked");
        }
    }

    let rig = Rig::new(Script::default());
    let job = claimed(7, None);
    let (log, mut lines) = tokio::sync::mpsc::channel(16);
    let build = Build {
        claimed: &job,
        input: input(),
        log,
        uploads: &Panics,
        ssh: None,
    };
    let drain = async { while lines.recv().await.is_some() {} };
    let building = std::panic::AssertUnwindSafe(rig.executor.build(build)).catch_unwind();
    let (ended, ()) = tokio::join!(building, drain);
    assert!(ended.is_err(), "the panic should reach the runner");
    assert_cleaned_up(&rig, 7);
}

#[tokio::test(start_paused = true)]
async fn leftover_pods_are_swept_at_startup() {
    let rig = Rig::new(Script::default());
    rig.cluster.leave(&pod_name(3), status::building());
    rig.cluster
        .leave(&pod_name(4), status::built(0, "Completed"));
    assert_eq!(rig.executor.sweep().await.unwrap(), 2);
    assert_eq!(rig.cluster.calls(), [Call::Sweep(Sweep::Namespace)]);
    assert!(!rig.cluster.has_pod(&pod_name(3)));
    assert!(!rig.cluster.has_pod(&pod_name(4)));
    assert_eq!(rig.executor.sweep().await.unwrap(), 0);
}

/// The startup sweep also deletes the pods of a dispatcher whose identity is
/// gone, since a builds namespace is one dispatcher's, and leaves a pod no
/// dispatcher made.
#[tokio::test(start_paused = true)]
async fn the_startup_sweep_takes_any_dispatchers_pods() {
    let rig = Rig::new(Script::default());
    rig.cluster.leave(&pod_name(3), status::building());
    let lost = "aurcache-build-fedcba9876543210-4";
    rig.cluster.leave_stranger(
        lost,
        &[
            ("app.kubernetes.io/managed-by", "aurcache-worker-kube"),
            ("aurcache.io/dispatcher", "fedcba9876543210"),
        ],
    );
    rig.cluster.leave_stranger(
        "unrelated",
        &[("aurcache.io/dispatcher", "fedcba9876543210")],
    );
    assert_eq!(rig.executor.sweep().await.unwrap(), 2);
    assert!(!rig.cluster.has_pod(&pod_name(3)));
    assert!(!rig.cluster.has_stranger(lost));
    assert!(rig.cluster.has_stranger("unrelated"));
}

/// The sweep as the dispatcher stops deletes its own pods and no other
/// dispatcher's: another's builds may still be running, in a namespace the
/// operator shared against the advice.
#[tokio::test(start_paused = true)]
async fn the_sweep_on_leaving_takes_only_this_dispatchers_pods() {
    let rig = Rig::new(Script::default());
    rig.cluster.leave(&pod_name(3), status::building());
    let other = "aurcache-build-fedcba9876543210-4";
    rig.cluster.leave_stranger(
        other,
        &[
            ("app.kubernetes.io/managed-by", "aurcache-worker-kube"),
            ("aurcache.io/dispatcher", "fedcba9876543210"),
        ],
    );
    assert_eq!(executor::leave(&*rig.cluster).await.unwrap(), 1);
    assert_eq!(rig.cluster.calls(), [Call::Sweep(Sweep::Own)]);
    assert!(!rig.cluster.has_pod(&pod_name(3)));
    assert!(rig.cluster.has_stranger(other));
}

/// A pod no build holds is deleted before the next claim: one whose delete
/// failed.
#[tokio::test(start_paused = true)]
async fn pods_nobody_holds_are_deleted_before_a_claim() {
    let rig = Rig::new(Script::default());
    rig.cluster.leave_build(3, status::built(0, "Completed"));
    assert!(rig.executor.ready_for_work(0).await);
    let gone = |name: &str| Call::Delete {
        name: name.to_string(),
        grace: aurcache_worker_kube::cluster::PROMPT,
    };
    assert_eq!(rig.cluster.deletions(), [gone(&pod_name(3))]);
}

/// A pod its own build deleted is not deleted again, nor reported as nobody's,
/// while the watch has yet to see it go.
#[tokio::test(start_paused = true)]
async fn a_pod_its_build_deleted_is_not_reaped_again() {
    let rig = Rig::new(Script::default());
    rig.cluster.lag_deletions();
    let built = rig.build(&claimed(7, None)).await;
    assert_eq!(built.report.outcome, BuildOutcome::Succeeded);
    assert!(rig.cluster.has_pod(&pod_name(7)));
    assert!(rig.executor.ready_for_work(0).await);
    let own = Call::Delete {
        name: pod_name(7),
        grace: GRACE,
    };
    assert_eq!(rig.cluster.deletions(), std::slice::from_ref(&own));

    // Gone from the watch, it is forgotten; back under the same name, it is
    // nobody's again.
    rig.cluster.remove(&pod_name(7));
    assert!(rig.executor.ready_for_work(0).await);
    rig.cluster.leave_build(7, status::built(0, "Completed"));
    assert!(rig.executor.ready_for_work(0).await);
    assert_eq!(
        rig.cluster.deletions(),
        [
            own,
            Call::Delete {
                name: pod_name(7),
                grace: aurcache_worker_kube::cluster::PROMPT,
            }
        ]
    );
}

/// The pod of a build that is running is that build's, whatever else is
/// asked of the cluster meanwhile.
#[tokio::test(start_paused = true)]
async fn the_pod_of_a_running_build_is_not_taken_for_an_orphan() {
    let rig = Rig::new(Script {
        report: None,
        after_log: AfterLog::Nothing,
        ..Script::default()
    });
    let job = claimed(7, None);
    let cancel = job.cancel.clone();
    let (built, ()) = tokio::join!(rig.build(&job), async {
        rig.cluster.until_exec("tail").await;
        assert!(rig.executor.ready_for_work(1).await);
        assert!(rig.cluster.deletions().is_empty());
        cancel.cancel();
    });
    assert_eq!(built.report.outcome, BuildOutcome::Canceled);
    assert_cleaned_up(&rig, 7);
}

#[tokio::test(start_paused = true)]
async fn no_claim_while_a_pod_is_unschedulable() {
    let rig = Rig::new(Script::default());
    assert!(rig.executor.ready_for_work(0).await);
    rig.cluster.leave(
        &pod_name(3),
        status::unschedulable("0/3 nodes are available"),
    );
    assert!(!rig.executor.ready_for_work(1).await);
    // A pod that is only starting does not hold claims back.
    rig.cluster.set_status(&pod_name(3), status::building());
    assert!(rig.executor.ready_for_work(1).await);
}

#[tokio::test(start_paused = true)]
async fn no_claim_while_the_cluster_refuses_a_build_pod() {
    let rig = Rig::new(Script::default());
    rig.cluster.refuse(Admission::Refused {
        message: "exceeded quota: builds, requested: pods=1, used: pods=4, limited: pods=4"
            .to_string(),
    });
    assert!(!rig.executor.ready_for_work(0).await);
    // The question is asked with a pod no build has, and creates nothing.
    assert_eq!(rig.cluster.calls(), [Call::Admits(pod_name(0))]);
    assert!(!rig.cluster.has_pod(&pod_name(0)));

    rig.cluster.admit();
    assert!(rig.executor.ready_for_work(0).await);
}

/// With `WORKER_KUBE_USER_NAMESPACES=true`, an API server that drops
/// `spec.hostUsers` (its `UserNamespacesSupport` gate off) would run every
/// build in its node's user namespace while the worker says otherwise. No
/// build is claimed while a dry run comes back without it, and a pod that
/// was created without it is deleted before anything runs in it.
#[tokio::test(start_paused = true)]
async fn no_build_without_the_user_namespace_it_asks_for() {
    let cfg = Config {
        user_namespaces: true,
        ..config()
    };
    let dropping = Script {
        host_users: HostUsers::Dropped,
        ..Script::default()
    };
    let rig = Rig::configured(dropping, Sink::default(), cfg.clone());
    assert!(!rig.executor.ready_for_work(0).await);
    assert!(!rig.cluster.has_pod(&pod_name(0)));

    let built = rig.build(&claimed(7, None)).await;
    assert_eq!(built.report.outcome, BuildOutcome::Interrupted);
    assert!(
        reason(&built).contains("UserNamespacesSupport"),
        "{}",
        reason(&built)
    );
    assert!(rig.cluster.execs().is_empty(), "nothing ran in the pod");
    assert!(!rig.cluster.has_pod(&pod_name(7)));
    assert!(
        rig.cluster
            .calls()
            .iter()
            .any(|call| matches!(call, Call::Delete { name, .. } if *name == pod_name(7)))
    );

    // Where it keeps the field, builds go on in their own user namespace.
    let rig = Rig::configured(Script::default(), Sink::default(), cfg);
    assert!(rig.executor.ready_for_work(0).await);
    let built = rig.build(&claimed(8, None)).await;
    assert_eq!(built.report.outcome, BuildOutcome::Succeeded);
    let host_users = rig.cluster.rendered()[0].spec.as_ref().unwrap().host_users;
    assert_eq!(host_users, Some(false));
}

/// A dry run that reached no API server says a create would reach none: the
/// build would be claimed only to be handed back.
#[tokio::test(start_paused = true)]
async fn no_claim_while_the_api_server_does_not_answer() {
    let rig = Rig::new(Script::default());
    rig.cluster.refuse(Admission::Unreached {
        message: "error trying to connect: connection refused".to_string(),
    });
    assert!(!rig.executor.ready_for_work(0).await);
    assert!(!rig.cluster.has_pod(&pod_name(0)));
    rig.cluster.admit();
    assert!(rig.executor.ready_for_work(0).await);
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

#[test]
fn the_executor_names_itself() {
    let rig = Rig::new(Script::default());
    assert_eq!(
        <support::KubeExecutorOverFake as Executor>::KIND,
        "kubernetes"
    );
    assert_eq!(
        rig.executor.describe_self(),
        "kubernetes (namespace aurcache-builds, builder registry.example/builder:latest)"
    );
}

/// Where an ssh-agent's socket would be, with no agent there: what is passed
/// on to it goes unanswered at once, which a test with its clock stopped can
/// wait for. The real agent is `aurcache-worker-container`'s to test.
fn no_ssh_agent() -> std::path::PathBuf {
    std::path::PathBuf::from("/nonexistent/aurcache-ssh-agent/agent.sock")
}

/// With a key, the dispatcher execs `ssh-link` into the build container
/// once, passes on to the build's ssh-agent what a login asks and refuses
/// the rest itself, and the build goes on.
#[tokio::test(start_paused = true)]
async fn the_ssh_link_is_opened_once_and_answered() {
    let mut rig = Rig::new(Script {
        after_log: AfterLog::Nothing,
        ssh_link: SshLinkScript::Asks(vec![vec![11], vec![22]]),
        ..Script::default()
    });
    rig.ssh = Some(no_ssh_agent());
    let built = rig.build(&claimed(7, None)).await;
    assert_eq!(
        built.report.outcome,
        BuildOutcome::Succeeded,
        "{}",
        built.log
    );
    let links: Vec<_> = rig
        .cluster
        .execs()
        .into_iter()
        .filter(|exec| exec.contains("ssh-link"))
        .collect();
    assert_eq!(
        links,
        ["build: ssh-link --socket /work/scratch/ssh-link/link.sock"]
    );
    let answers = rig.cluster.ssh_answers();
    assert_eq!(answers.len(), 1);
    // The request for the identities went to the agent, which is not there;
    // the lock request was refused without being passed on.
    assert_eq!(answers[0][0], [5]);
    assert_eq!(answers[0][1], [5]);
    assert!(
        built.log.contains(
            "[worker] the SSH link closed: 0 signature(s) made, 1 request(s) refused, \
             1 request(s) ssh-agent did not answer ("
        ),
        "{}",
        built.log
    );
    assert_cleaned_up(&rig, 7);
}

/// An exec that fails is not tried again: the agent, waiting for its link,
/// ends the build itself.
#[tokio::test(start_paused = true)]
async fn a_failed_ssh_link_is_not_opened_again() {
    let mut rig = Rig::new(Script {
        ssh_link: SshLinkScript::Fails,
        ..Script::default()
    });
    rig.ssh = Some(no_ssh_agent());
    let built = rig.build(&claimed(7, None)).await;
    let links = rig
        .cluster
        .execs()
        .into_iter()
        .filter(|exec| exec.contains("ssh-link"))
        .count();
    assert_eq!(links, 1);
    assert!(
        built.log.contains("the exec of ssh-link failed"),
        "{}",
        built.log
    );
}

/// Without a key, no link.
#[tokio::test(start_paused = true)]
async fn no_ssh_link_without_a_key() {
    let rig = Rig::new(Script::default());
    rig.build(&claimed(7, None)).await;
    assert!(
        !rig.cluster
            .execs()
            .iter()
            .any(|exec| exec.contains("ssh-link"))
    );
}

/// A key that cannot be read holds claims back, and claiming resumes once it
/// can be: every build would otherwise end without it and be queued again.
#[tokio::test(start_paused = true)]
async fn no_claim_while_the_ssh_key_cannot_be_read() {
    let dir = tempfile::tempdir().unwrap();
    let key = dir.path().join("ssh-privatekey");
    let hosts = dir.path().join("known_hosts");
    let mut cfg = support::config();
    cfg.credential = Some(aurcache_worker_container::ssh::CredentialFiles {
        key: key.clone(),
        known_hosts: hosts.clone(),
    });
    let cluster = support::FakeCluster::new(Script::default());
    let executor = aurcache_worker_kube::executor::KubeExecutor::new(cfg, Arc::clone(&cluster));
    assert!(!executor.ready_for_work(0).await, "no key file");
    assert!(
        cluster.calls().is_empty(),
        "nothing is asked of the cluster without a key"
    );

    let made = std::process::Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(&key)
        .status()
        .expect("ssh-keygen");
    assert!(made.success());
    std::fs::write(&hosts, "example.test ssh-ed25519 AAAA\n").unwrap();
    assert!(executor.ready_for_work(0).await, "the key reads again");
}
