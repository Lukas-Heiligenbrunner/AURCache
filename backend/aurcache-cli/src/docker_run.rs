//! The `docker run` commands behind `aurcli setup server` and `setup worker`.
//!
//! The local pair deliberately reproduces what the bundled compose file does:
//! one network, and one shared `enroll` volume that makes the worker
//! auto-approved. Without that a local install would end with the user hunting
//! for the approve button, which is the single most common first-run stumble.

use crate::compose::{ENROLLMENT_DIR, SERVER_SERVICE, WorkerEnv};
use anyhow::{Context, Result, bail};
use aurcache_common::ports::{AURCACHE_HTTP_PORT, AURCACHE_MIRROR_PORT, AURCACHE_WORKER_PORT};
use aurcache_common::shell::quote_argument;
use serde::Serialize;
use std::process::Command;

/// Docker objects the local setup owns. Named, not anonymous: a recreated
/// container has to find the same identity and the same data, or the worker
/// re-enrolls as a stranger and the server forgets its CA. The server's
/// container is [`SERVER_SERVICE`], the name a worker beside it dials.
pub const NETWORK: &str = "aurcache";
pub const WORKER_CONTAINER: &str = "aurcache-worker";
pub const ENROLL_VOLUME: &str = "aurcache_enroll";
/// Where the host keeps its timezone, and where the server image reads it.
const LOCALTIME: &str = "/etc/localtime";

/// A `docker run` invocation, built before anything is executed so `--dry-run`
/// can print exactly what would happen.
#[derive(Debug, Clone, Serialize)]
pub struct DockerRun {
    pub container_name: String,
    pub image: String,
    pub args: Vec<String>,
}

impl DockerRun {
    /// The command as a user would type it, for `--dry-run` and for logs.
    #[must_use]
    pub fn command_line(&self) -> String {
        let mut parts = vec!["docker".to_string()];
        parts.extend(self.args.iter().map(|arg| quote_argument(arg)));
        parts.join(" ")
    }
}

/// What the server container is run with.
#[derive(Debug, Clone)]
pub struct ServerRunSpec {
    pub image: String,
    pub public_url: String,
    pub tls_sans: String,
    pub log_level: String,
    /// Extra `docker run` arguments, placed before the image.
    pub extra: Vec<String>,
}

/// What the server container needs: three ports, three volumes for state that
/// must survive a restart, and the enrollment directory that lets a local
/// worker approve itself.
#[must_use]
pub fn server_run(spec: &ServerRunSpec) -> DockerRun {
    let ServerRunSpec {
        image,
        public_url,
        tls_sans,
        log_level,
        extra,
    } = spec;
    let mut args: Vec<String> = vec![
        "run".into(),
        "-d".into(),
        "--name".into(),
        SERVER_SERVICE.into(),
        "--network".into(),
        NETWORK.into(),
        "--restart".into(),
        "unless-stopped".into(),
    ];

    for port in [
        AURCACHE_HTTP_PORT,
        AURCACHE_MIRROR_PORT,
        AURCACHE_WORKER_PORT,
    ] {
        args.push("-p".into());
        args.push(format!("{port}:{port}"));
    }

    for (key, value) in [
        ("LOG_LEVEL", log_level.as_str()),
        ("AURCACHE_TLS_SANS", tls_sans),
        ("AURCACHE_PUBLIC_URL", public_url),
        ("AURCACHE_ENROLLMENT_DIR", ENROLLMENT_DIR),
    ] {
        args.push("-e".into());
        args.push(format!("{key}={value}"));
    }

    // Everything the server keeps lives under /app: one volume persists the
    // database, the repository, the CA, the build logs and the source cache.
    args.push("-v".into());
    args.push("aurcache_data:/app".into());
    args.push("-v".into());
    args.push(format!("{ENROLL_VOLUME}:{ENROLLMENT_DIR}:ro"));

    // The timezone cron schedules are read in: this host's. `-e TZ` without a
    // value forwards `TZ` only if it is set here, so exporting it is how to
    // force one; otherwise the host's /etc/localtime decides. Mounted only
    // where it exists, since Docker would create a directory in its place.
    args.push("-e".into());
    args.push("TZ".into());
    if cfg!(target_os = "linux") && std::path::Path::new(LOCALTIME).is_file() {
        args.push("-v".into());
        args.push(format!("{LOCALTIME}:{LOCALTIME}:ro"));
    }

    args.extend(extra.iter().cloned());
    args.push(image.clone());

    DockerRun {
        container_name: SERVER_SERVICE.to_string(),
        image: image.clone(),
        args,
    }
}

/// What the worker container needs.
///
/// `privileged` and a tmpfs `/run` are not optional: devtools builds each
/// package in a systemd-nspawn chroot, which needs namespaces a plain container
/// forbids. The data volume is not optional either — without it a recreated
/// container loses its identity and re-enrolls as a fresh pending worker, which
/// looks exactly like nobody having approved it.
#[derive(Debug, Clone)]
pub struct WorkerRunSpec {
    pub image: String,
    pub container_name: String,
    pub env: WorkerEnv,
    pub log_level: String,
    /// Whether to join the local docker network, which is how a worker beside
    /// the server reaches it by container name.
    pub join_network: bool,
    /// Holds the identity and the storage pool, caches included.
    pub data_volume: String,
    /// Extra `docker run` arguments, placed before the image.
    pub extra: Vec<String>,
}

#[must_use]
pub fn worker_run(spec: &WorkerRunSpec) -> DockerRun {
    let WorkerRunSpec {
        image,
        container_name,
        env,
        log_level,
        join_network,
        data_volume,
        extra,
    } = spec;

    let mut args: Vec<String> = vec![
        "run".into(),
        "-d".into(),
        "--name".into(),
        container_name.clone(),
        "--restart".into(),
        "unless-stopped".into(),
        "--privileged".into(),
        "--tmpfs".into(),
        "/run".into(),
    ];

    if *join_network {
        args.push("--network".into());
        args.push(NETWORK.into());
    }

    args.push("-e".into());
    args.push(format!("RUST_LOG={log_level}"));
    for var in env.variables() {
        args.push("-e".into());
        args.push(var.to_string());
    }

    args.push("-v".into());
    args.push(format!("{data_volume}:/var/lib/aurcache-worker"));
    // The storage pool, when it is not the image in the data volume.
    if let Some(mapping) = env.pool.device_mapping() {
        args.push("--device".into());
        args.push(mapping);
    }
    if let Some(mapping) = env.pool.volume_mapping() {
        args.push("-v".into());
        args.push(mapping);
    }
    if env.enrollment_dir.is_some() {
        args.push("-v".into());
        args.push(format!("{ENROLL_VOLUME}:{ENROLLMENT_DIR}"));
    }

    args.extend(extra.iter().cloned());
    args.push(image.clone());

    DockerRun {
        container_name: container_name.clone(),
        image: image.clone(),
        args,
    }
}

/// The worker environment for joining a server somewhere else.
///
/// A fingerprint is not forced: on a trusted LAN trust-on-first-use is a
/// reasonable choice and the worker warns about it. Over anything else it
/// should be set, which is what the caller warns about.
#[must_use]
pub fn remote_worker_env(mut env: WorkerEnv, server_url: &str) -> WorkerEnv {
    env.url = Some(server_url.to_string());
    env.enrollment_dir = None;
    env
}

/// Create a docker object, treating "it already exists" as success.
///
/// Setup has to be safe to re-run: someone who adds a second worker should not
/// have to know whether the network already exists.
fn ensure_docker_object(kind: &str, name: &str) -> Result<()> {
    let inspect = docker(&[kind, "inspect", name])?;
    if inspect.status.success() {
        return Ok(());
    }
    let created = docker(&[kind, "create", name])?;
    if !created.status.success() {
        let stderr = String::from_utf8_lossy(&created.stderr);
        bail!("failed to create docker {kind} `{name}`: {}", stderr.trim());
    }
    Ok(())
}

fn docker(args: &[&str]) -> Result<std::process::Output> {
    Command::new("docker")
        .args(args)
        .output()
        .map_err(docker_error)
}

/// Make sure the network and the enrollment volume a server and the workers
/// beside it share exist.
pub fn ensure_local_objects() -> Result<()> {
    ensure_docker_object("network", NETWORK)?;
    ensure_docker_object("volume", ENROLL_VOLUME)
}

/// Run it, inheriting stdio so docker's own progress and errors reach the user.
pub fn execute(run: &DockerRun) -> Result<()> {
    let status = Command::new("docker")
        .args(&run.args)
        .status()
        .map_err(docker_error)
        .context("starting the container")?;

    if !status.success() {
        bail!(
            "docker run exited with status {}; if a container named `{}` already \
             exists, remove it with `docker rm -f {}`",
            status.code().unwrap_or(-1),
            run.container_name,
            run.container_name
        );
    }
    Ok(())
}

/// Docker failing to start at all, named for what to do about it.
fn docker_error(error: std::io::Error) -> anyhow::Error {
    if error.kind() == std::io::ErrorKind::NotFound {
        anyhow::anyhow!(
            "docker not found on PATH; install Docker, or use `--dry-run` \
             to print the command, or `setup compose` to write a file instead"
        )
    } else {
        anyhow::Error::new(error).context("failed to run docker")
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DockerRun, ENROLL_VOLUME, NETWORK, ServerRunSpec, WorkerRunSpec, quote_argument,
        remote_worker_env, server_run, worker_run,
    };
    use crate::compose::WorkerEnv;
    use crate::url::is_loopback;

    fn server(image: &str, extra: &[&str]) -> DockerRun {
        server_run(&ServerRunSpec {
            image: image.to_string(),
            public_url: "http://localhost:8081".to_string(),
            tls_sans: "aurcache,localhost".to_string(),
            log_level: "info".to_string(),
            extra: extra.iter().map(ToString::to_string).collect(),
        })
    }

    fn args_of(run: &DockerRun) -> String {
        run.args.join(" ")
    }

    fn spec(env: WorkerEnv, join_network: bool, data: &str) -> WorkerRunSpec {
        WorkerRunSpec {
            image: "img".to_string(),
            container_name: "w".to_string(),
            env,
            log_level: "info".to_string(),
            join_network,
            data_volume: data.to_string(),
            extra: Vec::new(),
        }
    }

    #[test]
    fn the_server_publishes_all_three_ports() {
        let run = server("img", &[]);
        let args = args_of(&run);
        for port in ["8080:8080", "8081:8081", "8083:8083"] {
            assert!(args.contains(port), "{args}");
        }
    }

    /// `-e TZ` with no value forwards the CLI's own `TZ` only when it is set,
    /// so an unset one leaves the host's /etc/localtime in charge.
    #[test]
    fn the_server_is_handed_the_hosts_timezone() {
        let run = server("img", &[]);
        assert!(
            run.args.windows(2).any(|pair| pair == ["-e", "TZ"]),
            "{:?}",
            run.args
        );
    }

    /// Everything a restart must not lose -- the CA, the database, the build
    /// logs -- is under /app, in one named volume.
    #[test]
    fn the_server_persists_its_state_in_named_volumes() {
        let run = server("img", &[]);
        let args = args_of(&run);
        assert!(args.contains("aurcache_data:/app"), "{args}");
    }

    /// The server only reads enrollment requests; a worker writes them.
    #[test]
    fn the_server_mounts_the_enrollment_volume_read_only() {
        let run = server("img", &[]);
        assert!(args_of(&run).contains(&format!("{ENROLL_VOLUME}:/enroll:ro")));
    }

    #[test]
    fn the_image_is_the_last_argument() {
        let run = server("myimage", &[]);
        assert_eq!(run.args.last().map(String::as_str), Some("myimage"));
    }

    #[test]
    fn passthrough_arguments_come_before_the_image() {
        let run = server("myimage", &["--pull=always"]);
        let position = run.args.iter().position(|a| a == "--pull=always").unwrap();
        assert_eq!(position, run.args.len() - 2, "{:?}", run.args);
    }

    /// Not optional: a chroot build needs namespaces a plain container forbids.
    #[test]
    fn a_worker_is_always_privileged_with_a_tmpfs_run() {
        let run = worker_run(&spec(WorkerEnv::default(), false, "d"));
        let args = args_of(&run);
        assert!(args.contains("--privileged"), "{args}");
        assert!(args.contains("--tmpfs /run"), "{args}");
    }

    /// A device or mount backing reaches the container at the path the worker
    /// is told about in its environment.
    #[test]
    fn a_worker_run_passes_its_pool_through() {
        let mut env = WorkerEnv::default();
        env.pool.backing = crate::compose::PoolBacking::Device("/dev/sdz".to_string());
        let args = args_of(&worker_run(&spec(env, false, "d")));
        assert!(
            args.contains("--device /dev/sdz:/dev/aurcache-pool"),
            "{args}"
        );
        assert!(args.contains("WORKER_POOL=/dev/aurcache-pool"), "{args}");

        let mut env = WorkerEnv::default();
        env.pool.backing = crate::compose::PoolBacking::Mount("/srv/pool".to_string());
        let args = args_of(&worker_run(&spec(env, false, "d")));
        assert!(
            args.contains("-v /srv/pool:/var/lib/aurcache-pool"),
            "{args}"
        );
        assert!(
            args.contains("WORKER_POOL=/var/lib/aurcache-pool"),
            "{args}"
        );
    }

    /// Without a named data volume a recreated worker re-enrolls as a stranger,
    /// which is indistinguishable from nobody having approved it.
    #[test]
    fn a_worker_always_persists_its_identity() {
        let run = worker_run(&spec(WorkerEnv::default(), false, "mydata"));
        let args = args_of(&run);
        assert!(args.contains("mydata:/var/lib/aurcache-worker"), "{args}");
        // The caches live in the storage pool, inside that volume.
        assert!(!args.contains("/var/cache/aurcache-worker"), "{args}");
    }

    /// The local pair reproduces the bundle: shared network, shared volume, no
    /// pin, and therefore no approval step.
    #[test]
    fn a_local_worker_joins_the_network_and_shares_the_enrollment_volume() {
        let env = WorkerEnv::default().beside_server();
        let run = worker_run(&spec(env, true, "d"));
        let args = args_of(&run);
        assert!(args.contains(&format!("--network {NETWORK}")), "{args}");
        assert!(args.contains(&format!("{ENROLL_VOLUME}:/enroll")), "{args}");
        assert!(
            args.contains("AURCACHE_URL=https://aurcache:8083"),
            "{args}"
        );
        assert!(!args.contains("CA_FINGERPRINT"), "{args}");
    }

    /// A remote worker has no volume to share, so it must not be given one to
    /// mount — the mount would succeed and enroll nothing.
    #[test]
    fn a_remote_worker_gets_no_enrollment_volume() {
        let env = remote_worker_env(WorkerEnv::default(), "https://build.example.com:8083");
        let run = worker_run(&spec(env, false, "d"));
        let args = args_of(&run);
        assert!(!args.contains(ENROLL_VOLUME), "{args}");
        assert!(!args.contains("AURCACHE_ENROLLMENT_DIR"), "{args}");
        assert!(
            args.contains("AURCACHE_URL=https://build.example.com:8083"),
            "{args}"
        );
    }

    #[test]
    fn a_supplied_fingerprint_survives_onto_a_remote_worker() {
        let env = remote_worker_env(
            WorkerEnv {
                ca_fingerprint: Some("abc".to_string()),
                ..WorkerEnv::default()
            },
            "https://build.example.com:8083",
        );
        let run = worker_run(&spec(env, false, "d"));
        assert!(args_of(&run).contains("AURCACHE_SERVER_CA_FINGERPRINT=abc"));
    }

    #[test]
    fn loopback_addresses_are_recognised_as_this_machine() {
        for host in ["localhost", "127.0.0.1", "::1", "[::1]", "0.0.0.0"] {
            assert!(is_loopback(host), "{host}");
        }
        assert!(!is_loopback("build.example.com"));
        assert!(!is_loopback("192.168.1.10"));
    }

    #[test]
    fn the_printed_command_quotes_only_what_needs_it() {
        assert_eq!(quote_argument("--privileged"), "--privileged");
        assert_eq!(
            quote_argument("AURCACHE_URL=https://h:8083"),
            "AURCACHE_URL=https://h:8083"
        );
        assert_eq!(quote_argument("a b"), "'a b'");
        assert_eq!(quote_argument("it's"), "'it'\"'\"'s'");
    }

    #[test]
    fn the_command_line_starts_with_docker_run() {
        let run = server("img", &[]);
        assert!(
            run.command_line().starts_with("docker run -d "),
            "{}",
            run.command_line()
        );
    }
}
