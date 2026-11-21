//! The containers this worker asks the engine for, rendered from what it
//! knows, so what a build may do is decided in one place that tests can read.
//!
//! A build container starts as root, with every capability dropped but the
//! seven the agent needs, `no-new-privileges`, and the build directory as its
//! only bind. The agent is its entrypoint and its PID 1: it takes the
//! directory, does the root step, and runs makepkg as an unprivileged user.
//! See `design/in-progress/container-workers.md`, "The Docker worker on the
//! agent".

use aurcache_build_agent::layout::{BIN_NAME, CAPABILITIES};
use aurcache_worker_container::arch::Platform;
use bollard::models::{
    ContainerCreateBody, EndpointSettings, HealthConfig, HostConfig, NetworkingConfig,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::config::Tunables;
use crate::network::NetworkPlan;

/// Where the build directory is bound inside the container.
pub const WORK: &str = "/work";
/// Where the build's own trees go: inside the container, so nothing the build
/// user owns is left in a host directory.
pub const SCRATCH: &str = "/var/lib/aurcache-build";
/// The directory under the build directory the agent is copied into.
pub const BIN_DIR: &str = "bin";

/// `/dev/shm` when no memory limit is set. Docker's own default, 64 MiB, is
/// too small for test suites that share memory between processes.
const DEFAULT_SHM: i64 = 2 << 30;

/// What one build container is made of.
pub struct Build<'a> {
    pub image: &'a str,
    /// The build directory, as the engine sees it.
    pub host_dir: &'a Path,
    pub tunables: Tunables,
    pub network: &'a NetworkPlan,
}

/// The agent's path inside the container.
#[must_use]
pub fn agent() -> PathBuf {
    Path::new(WORK).join(BIN_DIR).join(BIN_NAME)
}

/// The build container for `build`.
#[must_use]
pub fn render(build: &Build<'_>) -> ContainerCreateBody {
    let memory = build.tunables.memory_bytes();
    ContainerCreateBody {
        image: Some(build.image.to_string()),
        user: Some("0".to_string()),
        entrypoint: Some(vec![agent().display().to_string()]),
        cmd: Some(vec!["build".into(), "--work".into(), WORK.into()]),
        attach_stdout: Some(true),
        attach_stderr: Some(true),
        open_stdin: Some(false),
        // No health check of the image's: it would be one more exec into
        // the build, and the agent kills every one but the SSH link's.
        healthcheck: Some(HealthConfig {
            test: Some(vec!["NONE".to_string()]),
            ..Default::default()
        }),
        host_config: Some(HostConfig {
            auto_remove: Some(false),
            privileged: Some(false),
            // The agent must be PID 1 to collect everything the build left
            // running; a daemon configured with `"init": true` would put
            // another process there.
            init: Some(false),
            cap_drop: Some(vec!["ALL".to_string()]),
            cap_add: Some(CAPABILITIES.iter().map(ToString::to_string).collect()),
            security_opt: Some(vec!["no-new-privileges".to_string()]),
            binds: Some(vec![format!("{}:{WORK}", build.host_dir.display())]),
            nano_cpus: build.tunables.nano_cpus(),
            // Both, and equal: `MEMORY_LIMIT` is RAM and swap together, which
            // Docker spells as a `MemorySwap` total equal to the `Memory`
            // limit. `MemorySwap` alone is refused.
            memory,
            memory_swap: memory,
            // tmpfs pages count against the container's memory limit, so
            // `/dev/shm` may be as large as that limit without raising it.
            shm_size: Some(memory.unwrap_or(DEFAULT_SHM)),
            network_mode: build.network.network_mode(),
            ..Default::default()
        }),
        networking_config: endpoint(build.network),
        ..Default::default()
    }
}

/// A short-lived container that empties the build directory at `host_dir`,
/// for when a stopped build left it root's.
///
/// Root with what removing another user's files takes and nothing else, and
/// no network.
#[must_use]
pub fn cleaner(image: &str, host_dir: &Path) -> ContainerCreateBody {
    ContainerCreateBody {
        image: Some(image.to_string()),
        user: Some("0".to_string()),
        entrypoint: Some(vec!["find".to_string()]),
        cmd: Some(
            [WORK, "-mindepth", "1", "-delete"]
                .map(ToString::to_string)
                .to_vec(),
        ),
        attach_stdout: Some(false),
        attach_stderr: Some(false),
        host_config: Some(HostConfig {
            auto_remove: Some(false),
            privileged: Some(false),
            cap_drop: Some(vec!["ALL".to_string()]),
            cap_add: Some(vec!["DAC_OVERRIDE".to_string(), "FOWNER".to_string()]),
            security_opt: Some(vec!["no-new-privileges".to_string()]),
            binds: Some(vec![format!("{}:{WORK}", host_dir.display())]),
            network_mode: Some("none".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// The engine's spelling of `platform`: `linux/arm/v7`.
#[must_use]
pub fn platform_name(platform: Platform) -> String {
    match platform.variant {
        Some(variant) => format!("linux/{}/{variant}", platform.arch),
        None => format!("linux/{}", platform.arch),
    }
}

/// The named network a container joins, when the plan names one.
fn endpoint(network: &NetworkPlan) -> Option<NetworkingConfig> {
    network.endpoint_network().map(|name| NetworkingConfig {
        endpoints_config: Some(HashMap::from([(
            name.to_string(),
            EndpointSettings::default(),
        )])),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use aurcache_worker_container::arch::platform;

    fn build_with(tunables: Tunables, network: &NetworkPlan) -> ContainerCreateBody {
        render(&Build {
            image: "builder:test",
            host_dir: Path::new("/srv/builds/7"),
            tunables,
            network,
        })
    }

    fn unlimited() -> Tunables {
        Tunables {
            cpu_limit: 0,
            memory_limit: -1,
        }
    }

    fn rendered() -> ContainerCreateBody {
        build_with(unlimited(), &NetworkPlan::Default)
    }

    fn host(body: &ContainerCreateBody) -> &HostConfig {
        body.host_config.as_ref().unwrap()
    }

    #[test]
    fn it_is_never_privileged() {
        for body in [
            rendered(),
            cleaner("builder:test", Path::new("/srv/builds/7")),
        ] {
            assert_ne!(host(&body).privileged, Some(true));
        }
    }

    #[test]
    fn root_has_exactly_the_seven_capabilities() {
        let body = rendered();
        assert_eq!(
            host(&body).cap_drop.as_deref(),
            Some(&["ALL".to_string()][..])
        );
        let added = host(&body).cap_add.clone().unwrap();
        assert_eq!(added, CAPABILITIES.map(String::from));
        assert_eq!(body.user.as_deref(), Some("0"));
    }

    #[test]
    fn nothing_regains_a_privilege() {
        let body = rendered();
        assert_eq!(
            host(&body).security_opt.as_deref(),
            Some(&["no-new-privileges".to_string()][..])
        );
    }

    #[test]
    fn the_images_health_check_is_turned_off() {
        let test = rendered().healthcheck.and_then(|health| health.test);
        assert_eq!(test.as_deref(), Some(&["NONE".to_string()][..]));
    }

    /// The agent refuses to run unless it is PID 1.
    #[test]
    fn the_engine_puts_no_init_before_the_agent() {
        assert_eq!(host(&rendered()).init, Some(false));
    }

    #[test]
    fn the_agent_is_the_entrypoint_and_builds_in_work() {
        let body = rendered();
        assert_eq!(
            body.entrypoint.as_deref(),
            Some(&["/work/bin/aurcache-build-agent".to_string()][..])
        );
        assert_eq!(
            body.cmd.as_deref(),
            Some(&["build", "--work", "/work"].map(String::from)[..])
        );
        assert_eq!(body.image.as_deref(), Some("builder:test"));
    }

    /// Nothing is bound over `/etc`: root in the container writes it.
    #[test]
    fn the_build_directory_is_the_only_bind() {
        let body = rendered();
        assert_eq!(
            host(&body).binds.as_deref(),
            Some(&["/srv/builds/7:/work".to_string()][..])
        );
        assert_eq!(host(&body).mounts, None);
        assert_eq!(body.volumes, None);
    }

    #[test]
    fn the_limits_reach_the_container_in_dockers_units() {
        let body = build_with(
            Tunables {
                cpu_limit: 2000,
                memory_limit: 512,
            },
            &NetworkPlan::Default,
        );
        let host = host(&body);
        assert_eq!(host.nano_cpus, Some(2_000_000_000));
        assert_eq!(host.memory, Some(512 << 20));
        assert_eq!(host.memory_swap, Some(512 << 20));
        assert_eq!(host.shm_size, Some(512 << 20));
    }

    #[test]
    fn unlimited_sets_no_limit_and_a_usable_shm() {
        let body = rendered();
        let host = host(&body);
        assert_eq!(host.nano_cpus, None);
        assert_eq!(host.memory, None);
        assert_eq!(host.memory_swap, None);
        assert_eq!(host.shm_size, Some(2 << 30));
    }

    #[test]
    fn the_network_plan_is_followed() {
        let shared = build_with(unlimited(), &NetworkPlan::ShareOwnNamespace("abc".into()));
        assert_eq!(host(&shared).network_mode.as_deref(), Some("container:abc"));
        assert!(shared.networking_config.is_none());

        let joined = build_with(unlimited(), &NetworkPlan::JoinNetwork("net".into()));
        assert_eq!(host(&joined).network_mode, None);
        let endpoints = joined
            .networking_config
            .and_then(|config| config.endpoints_config)
            .unwrap();
        assert_eq!(endpoints.keys().collect::<Vec<_>>(), ["net"]);
    }

    #[test]
    fn the_cleaner_empties_the_directory_with_little_else() {
        let body = cleaner("builder:test", Path::new("/srv/builds/7"));
        let host = host(&body);
        assert_eq!(body.user.as_deref(), Some("0"));
        assert_eq!(host.cap_drop.as_deref(), Some(&["ALL".to_string()][..]));
        assert_eq!(
            host.cap_add.as_deref(),
            Some(&["DAC_OVERRIDE".to_string(), "FOWNER".to_string()][..])
        );
        assert_eq!(host.network_mode.as_deref(), Some("none"));
        assert_eq!(
            host.binds.as_deref(),
            Some(&["/srv/builds/7:/work".to_string()][..])
        );
        assert_eq!(
            body.cmd.as_deref(),
            Some(&["/work", "-mindepth", "1", "-delete"].map(String::from)[..])
        );
    }

    #[test]
    fn platforms_are_spelled_as_the_engine_reads_them() {
        assert_eq!(platform_name(platform("x86_64").unwrap()), "linux/amd64");
        assert_eq!(platform_name(platform("aarch64").unwrap()), "linux/arm64");
        assert_eq!(platform_name(platform("armv7h").unwrap()), "linux/arm/v7");
    }
}
