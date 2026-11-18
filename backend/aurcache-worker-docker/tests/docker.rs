//! The rendered build container against the local Docker daemon, with a
//! statically built agent: a package builds, and a build killed half-way
//! leaves nothing behind.
//!
//! Ignored by default. Run with
//!
//! ```text
//! AURCACHE_DOCKER_TESTS=1 WORKER_BUILD_AGENT=<static agent> AURCACHE_TEST_PREFIX=<prefix> \
//!     cargo test -p aurcache-worker-docker --test docker -- --ignored
//! ```
//!
//! `BUILDER_IMAGE` names the image (the published builder by default).

use aurcache_build_agent::layout::Work;
use aurcache_build_agent::report::Ended;
use aurcache_build_agent::spec::{BuildSpec, INTERFACE};
use aurcache_worker_container::arch::{Platform, platform};
use aurcache_worker_container::outcome::Ending;
use aurcache_worker_docker::cleanup::{self, Leftover};
use aurcache_worker_docker::config::{BuildDirs, Tunables};
use aurcache_worker_docker::container::{self, Build, SCRATCH};
use aurcache_worker_docker::ending::{Verdict, ending};
use aurcache_worker_docker::network::NetworkPlan;
use aurcache_worker_docker::workdir;
use bollard::Docker;
use bollard::query_parameters::{
    CreateContainerOptions, InspectContainerOptions, KillContainerOptions, StartContainerOptions,
    WaitContainerOptions,
};
use futures::StreamExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

const DEFAULT_IMAGE: &str = "ghcr.io/lukas-heiligenbrunner/aurcache-builder-v2:latest";

/// What one test needs from its environment; `None` when it should not run.
struct Setup {
    docker: Docker,
    agent: PathBuf,
    image: String,
    prefix: String,
    platform: Platform,
}

fn setup() -> Option<Setup> {
    if std::env::var_os("AURCACHE_DOCKER_TESTS").is_none() {
        eprintln!("skipped: AURCACHE_DOCKER_TESTS is not set");
        return None;
    }
    let agent = std::env::var_os("WORKER_BUILD_AGENT")
        .map(PathBuf::from)
        .expect("WORKER_BUILD_AGENT names the static agent");
    Some(Setup {
        docker: Docker::connect_with_unix_defaults().expect("the Docker socket"),
        agent,
        image: std::env::var("BUILDER_IMAGE").unwrap_or_else(|_| DEFAULT_IMAGE.to_string()),
        prefix: std::env::var("AURCACHE_TEST_PREFIX").unwrap_or_else(|_| "aurcache-test".into()),
        platform: platform(std::env::consts::ARCH).expect("a known architecture"),
    })
}

fn checkout() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// A fresh directory for one test, in Cargo's own temporary directory.
fn dirs(name: &str) -> BuildDirs {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("docker-{name}"));
    BuildDirs {
        host: dir.clone(),
        local: dir,
    }
}

/// `docker/testpkg/hello` as the server would send it: a gzipped tar with the
/// pkgbase as its top directory.
fn hello_source(stage: &Path) -> Vec<u8> {
    let tree = stage.join("hello");
    std::fs::create_dir_all(&tree).unwrap();
    for file in ["PKGBUILD", "hello.c"] {
        std::fs::copy(
            checkout().join("docker/testpkg/hello").join(file),
            tree.join(file),
        )
        .unwrap();
    }
    let archive = stage.join("source.tar.gz");
    let status = Command::new("tar")
        .arg("-czf")
        .arg(&archive)
        .arg("-C")
        .arg(stage)
        .arg("hello")
        .status()
        .unwrap();
    assert!(status.success());
    std::fs::read(archive).unwrap()
}

fn spec() -> BuildSpec {
    let arch = std::env::consts::ARCH;
    let repos = if arch == "x86_64" {
        &["core", "extra"][..]
    } else {
        &["core", "extra", "alarm"][..]
    };
    let mut pacman_conf = format!(
        "[options]\nDisableSandbox\nSigLevel = Never\nHoldPkg = pacman glibc\n\
         Architecture = {arch}\n"
    );
    for repo in repos {
        pacman_conf.push_str(&format!("\n[{repo}]\nInclude = /etc/pacman.d/mirrorlist\n"));
    }
    BuildSpec {
        interface: INTERFACE,
        pkgbase: "hello".into(),
        build_flags: ["--noconfirm", "--noprogressbar", "--nocolor"]
            .map(String::from)
            .to_vec(),
        makepkg_conf: "PACKAGER='AURCache <aurcache@localhost>'\nOPTIONS+=(!debug)\n".into(),
        pacman_upgrade_conf: pacman_conf.clone(),
        pacman_conf,
        mirrorlist: None,
        pgp_keys: vec![],
        keyserver: "hkps://keyserver.ubuntu.com".into(),
        vcs_sources: vec![],
        cpus: Some(2.0),
        scratch: SCRATCH.into(),
    }
}

/// Create and start the build container for `dirs`; its id.
async fn start(setup: &Setup, name: &str, dirs: &BuildDirs) -> String {
    let network = NetworkPlan::Default;
    let body = container::render(&Build {
        image: &setup.image,
        host_dir: &dirs.host,
        tunables: Tunables {
            cpu_limit: 2000,
            memory_limit: 4096,
        },
        network: &network,
    });
    cleanup::remove_container(&setup.docker, name).await;
    let created = setup
        .docker
        .create_container(
            Some(CreateContainerOptions {
                name: Some(name.to_string()),
                platform: container::platform_name(setup.platform),
            }),
            body,
        )
        .await
        .expect("the build container is created");
    setup
        .docker
        .start_container(&created.id, None::<StartContainerOptions>)
        .await
        .expect("the build container starts");
    created.id
}

/// Wait for a container; its exit status.
async fn wait(docker: &Docker, id: &str) -> Option<i64> {
    match docker
        .wait_container(id, None::<WaitContainerOptions>)
        .next()
        .await
    {
        Some(Ok(res)) => Some(res.status_code),
        Some(Err(bollard::errors::Error::DockerContainerWaitError { code, .. })) => Some(code),
        _ => None,
    }
}

async fn verdict(docker: &Docker, id: &str, status: Option<i64>) -> Verdict {
    let state = docker
        .inspect_container(id, None::<InspectContainerOptions>)
        .await
        .unwrap()
        .state
        .unwrap();
    Verdict {
        status,
        oom_killed: state.oom_killed.unwrap_or(false),
    }
}

async fn clear(setup: &Setup, dirs: &BuildDirs, name: &str) {
    cleanup::clear(
        &setup.docker,
        &Leftover {
            dirs,
            image: &setup.image,
            platform: setup.platform,
            name: format!("{name}-clean"),
        },
    )
    .await
    .expect("the build directory is removed");
    assert!(!dirs.local.exists(), "{} is left", dirs.local.display());
}

#[tokio::test]
#[ignore = "needs Docker and a static agent: AURCACHE_DOCKER_TESTS=1"]
async fn a_package_builds_and_its_directory_is_removed() {
    let Some(setup) = setup() else { return };
    let dirs = dirs("hello");
    let stage = dirs.local.with_extension("stage");
    let _ = std::fs::remove_dir_all(&stage);
    let source = hello_source(&stage);
    workdir::prepare(&dirs.local, &setup.agent, &spec(), &source).unwrap();

    let name = format!("{}-docker-hello", setup.prefix);
    let id = start(&setup, &name, &dirs).await;
    let status = wait(&setup.docker, &id).await;
    let verdict = verdict(&setup.docker, &id, status).await;
    cleanup::remove_container(&setup.docker, &id).await;

    assert_eq!(status, Some(0), "the agent wrote a report");
    let Ending::Reported(report) = ending(workdir::report(&dirs.local), verdict) else {
        panic!("expected a report");
    };
    assert_eq!(report.ended, Ended::Exited { code: 0 });
    assert!(report.peak_memory_bytes.is_some_and(|bytes| bytes > 0));
    let packages: Vec<_> = std::fs::read_dir(Work::new(&dirs.local).pkg())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(packages.len(), 1, "{packages:?}");
    assert!(
        packages[0].starts_with("aurcache-hello-1.0.0-1-"),
        "{packages:?}"
    );

    clear(&setup, &dirs, &name).await;
    std::fs::remove_dir_all(&stage).unwrap();
}

#[tokio::test]
#[ignore = "needs Docker and a static agent: AURCACHE_DOCKER_TESTS=1"]
async fn a_killed_build_leaves_nothing_behind() {
    let Some(setup) = setup() else { return };
    let dirs = dirs("killed");
    let stage = dirs.local.with_extension("stage");
    let _ = std::fs::remove_dir_all(&stage);
    let source = hello_source(&stage);
    workdir::prepare(&dirs.local, &setup.agent, &spec(), &source).unwrap();

    let name = format!("{}-docker-killed", setup.prefix);
    let id = start(&setup, &name, &dirs).await;
    // Long enough for the agent to have taken the directory for root.
    tokio::time::sleep(Duration::from_secs(3)).await;
    setup
        .docker
        .kill_container(&id, None::<KillContainerOptions>)
        .await
        .unwrap();
    let status = wait(&setup.docker, &id).await;
    let verdict = verdict(&setup.docker, &id, status).await;
    cleanup::remove_container(&setup.docker, &id).await;

    assert_eq!(status, Some(137));
    assert!(matches!(
        ending(workdir::report(&dirs.local), verdict),
        Ending::Unreported { .. }
    ));
    assert_eq!(
        workdir::remove(&dirs.local).unwrap(),
        workdir::Removal::NotPermitted,
        "the stopped build left its directory root's"
    );

    clear(&setup, &dirs, &name).await;
    std::fs::remove_dir_all(&stage).unwrap();
}
