use log::info;
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

pub async fn startup_tasks() -> anyhow::Result<()> {
    info!("{}", START_BANNER);

    for cs in CONTAINER_STORAGE_DIRS {
        if fs::remove_dir_all(cs).await.is_ok() {
            info!("Removed old container storage `{}`", cs);
        }
    }

    pacman_repo_utils::init_repo(&PathBuf::from("./repo"), "repo")
}
