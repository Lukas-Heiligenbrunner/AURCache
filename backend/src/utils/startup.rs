use log::{error, info, warn};
use std::path::PathBuf;
use tokio::fs;

const CONTAINER_STORAGE_DIRS: [&str; 2] = ["/run/containers/storage", "/run/libpod"];
const START_BANNER: &str = r"
          _    _ _____   _____           _
     /\  | |  | |  __ \ / ____|         | |
    /  \ | |  | | |__) | |     __ _  ___| |__   ___
   / /\ \| |  | |  _  /| |    / _` |/ __| '_ \ / _ \
  / ____ \ |__| | | \ \| |___| (_| | (__| | | |  __/
 /_/    \_\____/|_|  \_\\_____\__,_|\___|_| |_|\___|
";

pub async fn startup_tasks() {
    info!("{}", START_BANNER);
    let latest_commit_sha = option_env!("LATEST_COMMIT_SHA").unwrap_or("dev");
    info!("Version: {}#{}", env!("CARGO_PKG_VERSION"), latest_commit_sha);

    #[cfg(debug_assertions)]
    warn!("This is a dev build! Consider using a stable release.");

    for cs in CONTAINER_STORAGE_DIRS {
        if fs::remove_dir_all(cs).await.is_ok() {
            info!("Removed old container storage `{}`", cs);
        }
    }

    if let Err(e) = pacman_repo_utils::init_repo(&PathBuf::from("./repo"), "repo") {
        error!("Failed to initialize pacman repo: {:?}", e);
    }
}
