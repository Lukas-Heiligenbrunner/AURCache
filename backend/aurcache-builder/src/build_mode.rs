use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::{env, fs};
use tokio::sync::OnceCell;
use tracing::{info, warn};

pub enum BuildMode {
    DinD(DinDBuildconfig),
    Host(HostBuildconfig),
}

pub struct HostBuildconfig {
    pub mirrorlist_path_host: String,
    pub mirrorlist_path_aurcache: String,

    /// dir on docker host
    pub build_artifact_dir_host: String,
    /// dir inside aurcache
    pub build_artifact_dir_aurcache: String,
}

pub struct DinDBuildconfig {
    pub mirrorlist_path: String,
    /// package build path in aurcache container
    pub build_path: String,
}

#[must_use]
pub fn get_build_mode() -> BuildMode {
    let current_dir = env::current_dir().expect("Failed to get current working directory");

    if let Ok(v) = env::var("BUILD_ARTIFACT_DIR") {
        let build_artifact_dir_aurcache = current_dir.join("builds");

        let build_artifact_dir_host = v.clone();
        let mirrorlist_path_aurcache = format!(
            "{}/config/pacman_x86_64",
            build_artifact_dir_aurcache.display()
        );

        let mirrorlist_path_host = match env::var("MIRRORLIST_PATH_X86_64") {
            Ok(v) => v,
            Err(_) => format!("{v}/config/pacman_x86_64"),
        };

        // create config dir if not existing
        ensure_dir_exists(&build_artifact_dir_aurcache.join("config/pacman_x86_64"));
        // Builder containers bind-mount this dir and write packages into it;
        // make it world-writable so they can do so regardless of their uid.
        fs::set_permissions(&build_artifact_dir_aurcache, Permissions::from_mode(0o777))
            .expect("Failed to set permissions on build directory");

        let cfg = HostBuildconfig {
            mirrorlist_path_host,
            mirrorlist_path_aurcache,
            build_artifact_dir_host,
            build_artifact_dir_aurcache: build_artifact_dir_aurcache.display().to_string(),
        };
        BuildMode::Host(cfg)
    } else {
        let mirrorlist_path = if let Ok(v) = env::var("MIRRORLIST_PATH_X86_64") {
            v
        } else {
            // default mirrorlist dir is "./config/mirrorlist_x86_64"
            let mut config_dir = current_dir.clone();
            config_dir.push("config");
            config_dir.push("pacman_x86_64");

            // create config dir if not existing
            ensure_dir_exists(&config_dir);

            format!("{}", config_dir.display())
        };

        // in dind mode packages are stored to ./builds/ by default
        let mut aurcache_build_path = current_dir.clone();
        aurcache_build_path.push("builds");
        ensure_dir_exists(&aurcache_build_path);
        // Builder containers bind-mount this dir and write packages into it;
        // make it world-writable so they can do so regardless of their uid.
        fs::set_permissions(&aurcache_build_path, Permissions::from_mode(0o777))
            .expect("Failed to set permissions on build directory");

        ensure_dir_exists(&current_dir.join("repo"));

        let cfg = DinDBuildconfig {
            mirrorlist_path,
            build_path: aurcache_build_path.display().to_string(),
        };
        BuildMode::DinD(cfg)
    }
}

/// Repo connectivity config derived once at startup and reused for all builds.
pub struct RepoConfig {
    /// HTTP base URL at which builder containers reach the AURCache file server.
    pub url: String,
    /// Docker network that builder containers should join to reach `url` by IP.
    /// `None` when the URL is reachable without joining a specific network.
    pub builder_network: Option<String>,
    /// When `true`, builder containers are created with `--network=host` so they
    /// share the host's network namespace and can reach `url` via `localhost`.
    /// Used in DinD mode where the Podman bridge gateway is unreachable from
    /// inner containers due to netavark/iptables interaction inside Docker.
    pub host_network: bool,
}

static REPO_CONFIG: OnceCell<RepoConfig> = OnceCell::const_new();

/// Detect repo connectivity config once at startup.
///
/// Resolution order:
/// 1. `AURCACHE_REPO_URL` env var — explicit override, no `builder_network`.
/// 2. **Host mode** (`BUILD_ARTIFACT_DIR` set): inspect own container via
///    `$HOSTNAME`, find the first non-default-bridge network, use own IP on
///    that network.  Builder containers are attached to the same network so
///    they can reach AURCache directly without port exposure on the host.
/// 3. **DinD mode**: inspect the `bridge` network gateway.  Builder containers
///    created by the internal Docker daemon reach AURCache at the gateway IP.
pub async fn get_repo_config(docker: &bollard::Docker) -> anyhow::Result<&'static RepoConfig> {
    REPO_CONFIG
        .get_or_try_init(|| detect_repo_config(docker))
        .await
}

async fn detect_repo_config(docker: &bollard::Docker) -> anyhow::Result<RepoConfig> {
    if let Ok(url) = env::var("AURCACHE_REPO_URL") {
        info!("Using AURCACHE_REPO_URL={url}");
        return Ok(RepoConfig {
            url,
            builder_network: None,
            host_network: false,
        });
    }

    // Host mode: inspect own container to find the Docker network and
    // IP that builder containers should use to reach AURCache directly.
    if env::var("BUILD_ARTIFACT_DIR").is_ok()
        && let Ok(hostname) = env::var("HOSTNAME")
    {
        match docker.inspect_container(&hostname, None).await {
            Ok(info) => {
                let networks = info
                    .network_settings
                    .and_then(|ns| ns.networks)
                    .unwrap_or_default();

                for (network_name, endpoint) in networks {
                    if network_name == "bridge" {
                        continue;
                    }
                    if let Some(ip) = endpoint.ip_address.filter(|ip| !ip.is_empty()) {
                        let url = format!(
                            "http://{}:{}",
                            ip,
                            aurcache_types::ports::AURCACHE_MIRROR_PORT
                        );
                        info!(
                            "AURCACHE_REPO_URL not set; using own IP {url} \
                                 on network {network_name}. \
                                 Set AURCACHE_REPO_URL explicitly to override."
                        );
                        return Ok(RepoConfig {
                            url,
                            builder_network: Some(network_name),
                            host_network: false,
                        });
                    }
                }
            }
            Err(e) => {
                warn!(
                    "Failed to inspect own container ({hostname}): {e}. \
                     Falling back to bridge gateway detection."
                );
            }
        }
    }

    // DinD mode: builder containers run inside Podman (inside Docker). The
    // Podman bridge gateway IP is technically the loopback of the aurcache
    // container but is unreachable due to netavark/iptables interaction inside
    // Docker's network namespace. Instead, use --network=host so builder
    // containers share the aurcache container's network namespace and can reach
    // the AURCache file server via localhost.
    let url = format!(
        "http://localhost:{}",
        aurcache_types::ports::AURCACHE_MIRROR_PORT
    );
    info!(
        "AURCACHE_REPO_URL not set; DinD mode detected. Builder containers will \
         use host networking and connect via {url}. \
         Set AURCACHE_REPO_URL explicitly to override."
    );
    Ok(RepoConfig {
        url,
        builder_network: None,
        host_network: true,
    })
}

/// Create a directory (and any missing parents) if it does not already exist.
fn ensure_dir_exists(dir: &Path) {
    fs::create_dir_all(dir)
        .expect("Failed to create directory. Maybe container directory is not writeable?");
}

impl BuildMode {
    /// Returns the host-side path to the `mirrorlist` file that should be
    /// bind-mounted into builder containers at `/etc/pacman.d/mirrorlist`.
    pub fn mirrorlist_source_path(&self) -> String {
        match self {
            BuildMode::Host(cfg) => format!("{}/mirrorlist", cfg.mirrorlist_path_host),
            BuildMode::DinD(cfg) => format!("{}/mirrorlist", cfg.mirrorlist_path),
        }
    }
}
