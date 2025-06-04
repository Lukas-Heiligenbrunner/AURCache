use log::info;
use std::env;

pub enum BuildMode {
    DinD(DinDBuildconfig),
    Host(HostBuildconfig),
}

pub struct HostBuildconfig {
    pub mirrorlist_path_host: String,
    pub mirrorlist_path_aurcache: String,

    // todo integrate build artifact path mappings with those paths
    #[allow(dead_code)]
    pub build_artifact_dir_host: String,
    #[allow(dead_code)]
    pub build_artifact_dir_aurcache: String,
}

pub struct DinDBuildconfig {
    pub mirrorlist_path: String,
}

pub fn get_build_mode() -> BuildMode {
    match env::var("BUILD_ARTIFACT_DIR") {
        Ok(v) => {
            let build_artifact_dir_aurcache = "/app/builds".to_string();
            let build_artifact_dir_host = v.clone();
            let mirrorlist_path_aurcache =
                format!("{}/config/pacman_x86_64", build_artifact_dir_aurcache);
            // todo handle artifact dir is docker volume and not abs path!!
            // todo maybe mirrorlist_path_aurcache also needs a change if env var is set?
            let mirrorlist_path_host = match env::var("MIRRORLIST_PATH_X86_64") {
                Ok(v) => v,
                Err(_) => format!("{}/config/pacman_x86_64", v),
            };

            // create config dir if not existing
            create_config_dir(format!(
                "{}/config/pacman_x86_64",
                build_artifact_dir_aurcache
            ));

            let cfg = HostBuildconfig {
                mirrorlist_path_host,
                mirrorlist_path_aurcache,
                build_artifact_dir_host,
                build_artifact_dir_aurcache,
            };
            BuildMode::Host(cfg)
        }
        Err(_) => {
            let mirrorlist_path = match env::var("MIRRORLIST_PATH_X86_64") {
                Ok(v) => v,
                Err(_) => {
                    // default mirrorlist dir is "./config/mirrorlist_x86_64"
                    let mut config_dir =
                        env::current_dir().expect("Failed to get current working directory");
                    config_dir.push("config");
                    config_dir.push("pacman_x86_64");

                    // create config dir if not existing
                    create_config_dir(config_dir.display().to_string());

                    format!("{}", config_dir.display())
                }
            };

            let cfg = DinDBuildconfig { mirrorlist_path };
            BuildMode::DinD(cfg)
        }
    }
}

/// create config dir if not existing
fn create_config_dir(config_dir: String) {
    if std::fs::metadata(config_dir.as_str()).is_err() {
        std::fs::create_dir_all(config_dir.as_str()).expect(
            "Failed to create config directory. Maybe container directory is not writeable?",
        );
        info!("Created default MIRRORLIST_PATH_X86_64: {}", config_dir);
    }
}
