use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt;
use std::{env, fs};
use tracing::info;

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
    /// host path to the pacman repo directory
    pub repo_host_path: String,
}

pub struct DinDBuildconfig {
    pub mirrorlist_path: String,
    /// package build path in aurcache container
    pub build_path: String,
    /// path to the pacman repo directory inside the aurcache container
    pub repo_path: String,
}

#[must_use]
pub fn get_build_mode() -> BuildMode {
    let current_dir = env::current_dir().expect("Failed to get current working directory");

    if let Ok(v) = env::var("BUILD_ARTIFACT_DIR") {
        let mut build_artifact_dir_aurcache = current_dir.clone();
        build_artifact_dir_aurcache.push("builds");
        let repo_dir_aurcache = current_dir.join("repo");

        let build_artifact_dir_host = v.clone();
        let mirrorlist_path_aurcache = format!(
            "{}/config/pacman_x86_64",
            build_artifact_dir_aurcache.display()
        );

        let mirrorlist_path_host = match env::var("MIRRORLIST_PATH_X86_64") {
            Ok(v) => v,
            Err(_) => format!("{v}/config/pacman_x86_64"),
        };

        // Derive repo host path from BUILD_ARTIFACT_DIR: repo is at ../repo relative to builds
        let build_parent = std::path::Path::new(&v)
            .parent()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| ".".to_string());
        let repo_host_path = format!("{build_parent}/repo");

        // The Docker daemon needs the host path, but from inside the AURCache
        // container we can only guarantee the mounted in-container repo path.
        ensure_dir_exists(repo_dir_aurcache.display().to_string());

        // create config dir if not existing
        ensure_dir_exists(format!(
            "{}/config/pacman_x86_64",
            build_artifact_dir_aurcache.display()
        ));

        let cfg = HostBuildconfig {
            mirrorlist_path_host,
            mirrorlist_path_aurcache,
            build_artifact_dir_host,
            build_artifact_dir_aurcache: build_artifact_dir_aurcache.display().to_string(),
            repo_host_path,
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
            ensure_dir_exists(config_dir.display().to_string());

            format!("{}", config_dir.display())
        };

        // in dind mode packages are stored to ./builds/ by default
        let mut aurcache_build_path = current_dir.clone();
        aurcache_build_path.push("builds");
        ensure_dir_exists(aurcache_build_path.display().to_string());

        let repo_path = format!("{}/repo", current_dir.display());

        let cfg = DinDBuildconfig {
            mirrorlist_path,
            build_path: aurcache_build_path.display().to_string(),
            repo_path,
        };
        BuildMode::DinD(cfg)
    }
}

/// Create a directory if it does not already exist.
fn ensure_dir_exists(dir: String) {
    if fs::metadata(dir.as_str()).is_err() {
        fs::create_dir_all(dir.as_str())
            .expect("Failed to create directory. Maybe container directory is not writeable?");
        _ = fs::set_permissions(dir.clone(), Permissions::from_mode(0o777));
        info!("Created dir: {dir}");
    }
}
