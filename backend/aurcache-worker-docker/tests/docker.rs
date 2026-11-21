//! The rendered build container against the local Docker daemon, with a
//! statically built agent: a package builds, a build killed half-way leaves
//! nothing behind, and what root in the container leaves in the build
//! directory does not lead the worker out of it.
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
        arch: arch.into(),
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
        ssh: None,
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
    assert!(
        matches!(
            workdir::remove(&dirs.local).await,
            workdir::Removal::Left(_)
        ),
        "the stopped build left its directory root's"
    );

    clear(&setup, &dirs, &name).await;
    std::fs::remove_dir_all(&stage).unwrap();
}

/// Run `script` in the container as root, once the agent has taken `/work`:
/// what a dependency's install scriptlet can do, with the container's own
/// capabilities. What it printed.
async fn as_root(docker: &Docker, id: &str, script: &str) -> String {
    use bollard::exec::{CreateExecOptions, StartExecOptions, StartExecResults};
    let exec = docker
        .create_exec(
            id,
            CreateExecOptions {
                attach_stdout: Some(true),
                attach_stderr: Some(true),
                user: Some("0".to_string()),
                cmd: Some(vec![
                    "/bin/sh".to_string(),
                    "-c".to_string(),
                    format!(
                        "while [ \"$(stat -c %u /work)\" != 0 ] || [ ! -d /work/out ]; do \
                         sleep 0.1; done; {script}"
                    ),
                ]),
                ..Default::default()
            },
        )
        .await
        .expect("the exec is created");
    let mut said = String::new();
    if let StartExecResults::Attached { mut output, .. } = docker
        .start_exec(&exec.id, None::<StartExecOptions>)
        .await
        .expect("the exec starts")
    {
        while let Some(Ok(chunk)) = output.next().await {
            said.push_str(&chunk.to_string());
        }
    }
    said
}

/// Root in the container cannot stop the agent: the stop it asks for is
/// ignored, and so is every signal the kernel would deliver to PID 1 from
/// inside, and it cannot read the agent's memory. The build goes on to its
/// report.
#[tokio::test]
#[ignore = "needs Docker and a static agent: AURCACHE_DOCKER_TESTS=1"]
async fn root_in_the_container_cannot_stop_the_agent() {
    let Some(setup) = setup() else { return };
    let dirs = dirs("pid1");
    let stage = dirs.local.with_extension("stage");
    let _ = std::fs::remove_dir_all(&stage);
    let source = hello_source(&stage);
    workdir::prepare(&dirs.local, &setup.agent, &spec(), &source).unwrap();

    let name = format!("{}-docker-pid1", setup.prefix);
    let id = start(&setup, &name, &dirs).await;
    let said = as_root(
        &setup.docker,
        &id,
        "for signal in TERM INT KILL; do kill -s $signal 1 && echo \"sent $signal\"; done; \
         sleep 1; kill -0 1 && echo 'pid 1 lives'; \
         head -c1 /proc/1/mem >/dev/null 2>&1 || echo 'memory: refused'; \
         cat /proc/1/environ >/dev/null 2>&1 || echo 'environ: refused'",
    )
    .await;
    let status = wait(&setup.docker, &id).await;
    let verdict = verdict(&setup.docker, &id, status).await;
    cleanup::remove_container(&setup.docker, &id).await;

    assert!(said.contains("sent TERM"), "{said}");
    assert!(said.contains("pid 1 lives"), "{said}");
    assert!(said.contains("memory: refused"), "{said}");
    assert!(said.contains("environ: refused"), "{said}");
    assert_eq!(status, Some(0), "the agent wrote a report: {said}");
    let Ending::Reported(report) = ending(workdir::report(&dirs.local), verdict) else {
        panic!("expected a report");
    };
    assert_eq!(report.ended, Ended::Exited { code: 0 });

    clear(&setup, &dirs, &name).await;
    std::fs::remove_dir_all(&stage).unwrap();
}

/// What root in a build container leaves in the build directory is read as
/// what the build could have written: a pipe where the report goes and a
/// `pkg/` that is a link to a directory of the worker's host. Neither holds
/// the worker up, and nothing of the host's is taken for a package.
#[tokio::test]
#[ignore = "needs Docker and a static agent: AURCACHE_DOCKER_TESTS=1"]
async fn what_root_leaves_in_the_build_directory_is_not_followed() {
    use aurcache_worker_core::artifacts::discover_artifacts;
    use workdir::Report;

    let Some(setup) = setup() else { return };
    let dirs = dirs("hostile");
    let stage = dirs.local.with_extension("stage");
    let _ = std::fs::remove_dir_all(&stage);
    let source = hello_source(&stage);
    let host_only = stage.join("host-only");
    std::fs::create_dir_all(&host_only).unwrap();
    std::fs::write(
        host_only.join("secret-1-1-x86_64.pkg.tar.zst"),
        b"a file of the worker's host, never in the container",
    )
    .unwrap();
    workdir::prepare(&dirs.local, &setup.agent, &spec(), &source).unwrap();

    let name = format!("{}-docker-hostile", setup.prefix);
    let id = start(&setup, &name, &dirs).await;
    let said = as_root(
        &setup.docker,
        &id,
        &format!(
            "mkfifo -m 666 /work/out/report.json && rm -rf /work/pkg && ln -s {} /work/pkg \
             && echo planted",
            host_only.display()
        ),
    )
    .await;
    assert!(said.contains("planted"), "{said}");
    // As the worker stops a build it cancels.
    setup
        .docker
        .kill_container(&id, None::<KillContainerOptions>)
        .await
        .unwrap();
    let status = wait(&setup.docker, &id).await;
    cleanup::remove_container(&setup.docker, &id).await;
    assert_eq!(status, Some(137));

    let local = dirs.local.clone();
    let read = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::task::spawn_blocking(move || workdir::report(&local)),
    )
    .await
    .expect("the report is refused at once, not waited on")
    .unwrap();
    let Report::Unreadable(why) = read else {
        panic!("a pipe was read as a report: {read:?}");
    };
    assert!(why.contains("a pipe"), "{why}");
    let refused = discover_artifacts(&Work::new(&dirs.local).pkg()).unwrap_err();
    assert!(refused.to_string().contains("a link"), "{refused}");

    clear(&setup, &dirs, &name).await;
    assert!(host_only.join("secret-1-1-x86_64.pkg.tar.zst").exists());
    std::fs::remove_dir_all(&stage).unwrap();
}

/// A package that asks the agent it is given three things: at its top level
/// and in `build()`, which keys it has; and in `build()`, for a signature
/// that is not a login (an SSHSIG, as `ssh-keygen -Y sign` makes), which
/// ssh-agent makes like any other.
const LINK_PKGBUILD: &str = r#"pkgname=aurcache-link-test
pkgver=1.0.0
pkgrel=1
pkgdesc="The Docker worker's SSH link, from inside"
arch=('x86_64' 'aarch64' 'armv7h')
license=('GPL-3.0-or-later')
if [[ -n ${SSH_AUTH_SOCK:-} ]]; then
  ssh-add -L | sed 's/^/listed: /'
fi
build() {
  echo "in build: ${SSH_AUTH_SOCK:-no socket named}"
  ssh-add -L | sed 's/^/listed in build: /'
  ssh-add -L >"$srcdir/key.pub"
  if echo payload | ssh-keygen -Y sign -f "$srcdir/key.pub" -n file >/dev/null 2>&1; then
    echo "in build: a signature that is no login was made"
  else
    echo "in build: a signature that is no login was refused"
  fi
}
package() {
  :
}
"#;

/// The link the worker opens with `docker exec` reaches the key while
/// makepkg runs: the build has it sign, a login or not, and is given no key.
#[tokio::test]
#[ignore = "needs Docker and a static agent: AURCACHE_DOCKER_TESTS=1"]
async fn the_ssh_link_serves_the_build_from_the_workers_ssh_agent() {
    use aurcache_build_agent::spec::SshAccess;
    use aurcache_worker_container::ssh::agent::Agent;
    use aurcache_worker_container::ssh::link::LinkEnd;
    use std::sync::Arc;

    let Some(setup) = setup() else { return };
    let dirs = dirs("link");
    let stage = dirs.local.with_extension("stage");
    let _ = std::fs::remove_dir_all(&stage);
    let tree = stage.join("aurcache-link-test");
    std::fs::create_dir_all(&tree).unwrap();
    std::fs::write(tree.join("PKGBUILD"), LINK_PKGBUILD).unwrap();
    let archive = stage.join("source.tar.gz");
    assert!(
        Command::new("tar")
            .arg("-czf")
            .arg(&archive)
            .arg("-C")
            .arg(&stage)
            .arg("aurcache-link-test")
            .status()
            .unwrap()
            .success()
    );
    let key_file = stage.join("key");
    assert!(
        Command::new("ssh-keygen")
            .args([
                "-q",
                "-t",
                "ed25519",
                "-N",
                "",
                "-C",
                "aurcache-link-test",
                "-f"
            ])
            .arg(&key_file)
            .status()
            .expect("ssh-keygen")
            .success()
    );
    let agent = Arc::new(Agent::start(&key_file).await.unwrap());
    let mut spec = spec();
    spec.pkgbase = "aurcache-link-test".into();
    spec.ssh = Some(SshAccess {
        known_hosts: "example.test ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl\n".into(),
    });
    workdir::prepare(
        &dirs.local,
        &setup.agent,
        &spec,
        &std::fs::read(&archive).unwrap(),
    )
    .unwrap();

    let name = format!("{}-docker-link", setup.prefix);
    let id = start(&setup, &name, &dirs).await;
    let link = tokio::spawn(aurcache_worker_docker::link::serve(
        setup.docker.clone(),
        id.clone(),
        Arc::clone(&agent),
    ));
    let status = wait(&setup.docker, &id).await;
    let served = tokio::time::timeout(Duration::from_secs(10), link)
        .await
        .expect("the link ends with the container")
        .unwrap();
    let logs = Command::new("docker").args(["logs", &id]).output().unwrap();
    let said = String::from_utf8_lossy(&logs.stdout).into_owned();
    cleanup::remove_container(&setup.docker, &id).await;

    assert_eq!(status, Some(0), "{said}");
    assert_eq!(served.end, LinkEnd::Closed, "{said}");
    // The agent lists the key as its `.pub` file has it.
    let public = std::fs::read_to_string(key_file.with_extension("pub")).unwrap();
    let key_line = public.trim();
    let listed: Vec<&str> = said
        .lines()
        .filter_map(|line| line.strip_prefix("listed: "))
        .map(str::trim)
        .collect();
    assert!(!listed.is_empty(), "makepkg did not list the key:\n{said}");
    assert!(listed.iter().all(|line| *line == key_line), "{said}");
    // The same in `build()`, to the end of makepkg.
    assert!(said.contains("in build: /"), "{said}");
    assert!(
        said.contains(&format!("listed in build: {key_line}")),
        "{said}"
    );
    assert!(
        said.contains("in build: a signature that is no login was made"),
        "{said}"
    );
    assert!(served.signed >= 1, "{served:?}");
    assert_eq!(served.unanswered, None, "{served:?}");
    // What the build was given names the hosts and holds no key.
    let given = std::fs::read_to_string(Work::new(&dirs.local).spec()).unwrap();
    assert!(given.contains("known_hosts"), "{given}");
    assert!(!given.contains("PRIVATE KEY"), "{given}");

    clear(&setup, &dirs, &name).await;
    std::fs::remove_dir_all(&stage).unwrap();
}
