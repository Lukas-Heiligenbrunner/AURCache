use log::info;
use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt;
use std::{env, fs};

pub enum BuildMode {
    DinD(DinDBuildconfig),
    Host(HostBuildconfig),
}

pub struct HostBuildconfig {
    pub mirrorlist_path_host: String,
    pub mirrorlist_path_aurcache: String,

    pub build_artifact_dir_host: String,
}

pub struct DinDBuildconfig {
    pub mirrorlist_path: String,
    /// package build path in aurcache container
    pub aurcache_build_path: String,
}

pub fn get_build_mode() -> BuildMode {
    let current_dir = env::current_dir().expect("Failed to get current working directory");

    match env::var("BUILD_ARTIFACT_DIR") {
        Ok(v) => {
            let mut build_artifact_dir_aurcache = current_dir;
            build_artifact_dir_aurcache.push("builds");

            let build_artifact_dir_host = v.clone();
            let mirrorlist_path_aurcache = format!(
                "{}/config/pacman_x86_64",
                build_artifact_dir_aurcache.display()
            );

            let mirrorlist_path_host = match env::var("MIRRORLIST_PATH_X86_64") {
                Ok(v) => v,
                Err(_) => format!("{}/config/pacman_x86_64", v),
            };

            // create config dir if not existing
            create_config_dir(format!(
                "{}/config/pacman_x86_64",
                build_artifact_dir_aurcache.display()
            ));

            let cfg = HostBuildconfig {
                mirrorlist_path_host,
                mirrorlist_path_aurcache,
                build_artifact_dir_host,
            };
            BuildMode::Host(cfg)
        }
        Err(_) => {
            let mirrorlist_path = match env::var("MIRRORLIST_PATH_X86_64") {
                Ok(v) => v,
                Err(_) => {
                    // default mirrorlist dir is "./config/mirrorlist_x86_64"
                    let mut config_dir = current_dir.clone();
                    config_dir.push("config");
                    config_dir.push("pacman_x86_64");

                    // create config dir if not existing
                    create_config_dir(config_dir.display().to_string());

                    format!("{}", config_dir.display())
                }
            };

            // in dind mode packages are stored to ./builds/ by default
            let mut aurcache_build_path = current_dir;
            aurcache_build_path.push("builds");
            create_config_dir(aurcache_build_path.display().to_string());

            let cfg = DinDBuildconfig {
                mirrorlist_path,
                aurcache_build_path: aurcache_build_path.display().to_string(),
            };
            BuildMode::DinD(cfg)
        }
    }
}

/// create config dir if not existing
fn create_config_dir(config_dir: String) {
    if fs::metadata(config_dir.as_str()).is_err() {
        fs::create_dir_all(config_dir.as_str()).expect(
            "Failed to create config directory. Maybe container directory is not writeable?",
        );
        _ = fs::set_permissions(config_dir.clone(), Permissions::from_mode(0o777));
        info!("Created dir: {}", config_dir);
    }
}
