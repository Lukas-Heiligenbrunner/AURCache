use log::info;
use std::path::PathBuf;
use tokio::fs;

static START_BANNER: &str = r"
          _    _ _____   _____           _
     /\  | |  | |  __ \ / ____|         | |
    /  \ | |  | | |__) | |     __ _  ___| |__   ___
   / /\ \| |  | |  _  /| |    / _` |/ __| '_ \ / _ \
  / ____ \ |__| | | \ \| |___| (_| | (__| | | |  __/
 /_/    \_\____/|_|  \_\\_____\__,_|\___|_| |_|\___|
";

pub async fn startup_tasks() -> anyhow::Result<()> {
    info!("{}", START_BANNER);

    if fs::remove_dir_all("/run/containers/storage").await.is_ok() {
        info!("Removed old container storage `/run/containers/storage`");
    }
    if fs::remove_dir_all("/run/libpod").await.is_ok() {
        info!("Removed old container storage `/run/libpod`");
    }

    pacman_repo_utils::init_repo(&PathBuf::from("./repo"), "repo")
}
