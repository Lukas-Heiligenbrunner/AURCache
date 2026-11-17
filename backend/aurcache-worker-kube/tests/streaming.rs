//! Packages relayed from a pod to the real worker listener.
//!
//! The server's worker protocol listener runs in-process and a worker enrolls
//! with it through the production client. The cluster is the fake one, whose
//! `send` yields a package a few bytes at a time. Nothing here is mocked
//! between the dispatcher and the server: the upload is the request the
//! server's route reads, and a build is published by the server's own
//! publishing.

mod support;

use aurcache_build_agent::report::Ended;
use aurcache_common::build_state::BuildState;
use aurcache_common::worker::BuildOutcome;
use aurcache_db::files;
use aurcache_worker_container::spec::SpecOptions;
use aurcache_worker_container::{input, spec};
use aurcache_worker_core::build_log::BuildLog;
use aurcache_worker_kube::executor::{Build, KubeExecutor};
use aurcache_worker_kube::transfer::Agent;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use std::io::Write;
use std::sync::Arc;
use support::server::Server;
use support::{
    AGENT_IMAGE, Content, FakeCluster, Package, Script, config, pod_name, report, status,
};

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
    let build = server.settled(1).await;
    assert_eq!(build.status, BuildState::Successful);
    assert_eq!(build.peak_memory, Some(3 << 30));
    assert_eq!(build.disk_workdir, Some(2 << 30));
    let published = server.repo_root.join("x86_64").join(&built.name);
    let Content::Bytes(bytes) = &built.content else {
        unreachable!("the package was made of bytes");
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
    println!("relaying {LARGE} bytes grew the process's peak memory by {grown} bytes");

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
