use log::{error, info};
use std::path::PathBuf;
use tokio::fs;

#[cfg(debug_assertions)]
use log::warn;

#[cfg(not(debug_assertions))]
#[cfg(target_arch = "x86_64")]
use {
    log::debug,
    std::fs::File,
    std::io::{BufRead, BufReader, Write},
    std::path::Path,
};

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
    info!(
        "Version: {}#{}",
        env!("CARGO_PKG_VERSION"),
        latest_commit_sha
    );

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

    // disable on debug builds since annoying bc. of root permissions
    #[cfg(not(debug_assertions))]
    #[cfg(target_arch = "x86_64")]
    init_qemu_binfmt().await.unwrap();
}

/// This is required to initialize the binfmt configuration for QEMU on x86_64 correctly
/// arm64 is not supported by qemu binfmt
/// see https://stackoverflow.com/questions/75954301/using-sudo-in-podman-with-qemu-architecture-emulation-leads-to-sudo-effective-u
#[cfg(not(debug_assertions))]
#[cfg(target_arch = "x86_64")]
async fn init_qemu_binfmt() -> anyhow::Result<()> {
    let source_dir = Path::new("/usr/lib/binfmt.d");
    let target_dir = Path::new("/etc/binfmt.d");

    // Iterate over all .conf files in the source directory
    for entry in std::fs::read_dir(source_dir)? {
        let entry = entry?;
        let path = entry.path();

        if path.extension().and_then(|s| s.to_str()) == Some("conf") {
            let file = File::open(&path)?;
            let reader = BufReader::new(file);

            // Create the target file path
            let target_path = target_dir.join(path.file_name().unwrap());
            let mut target_file = File::create(&target_path)?;

            for mut line in reader.lines().map_while(Result::ok) {
                line.push('C');
                target_file.write_all(line.as_bytes())?;
            }

            debug!("Created qemu binfmt config: {}", path.display());
        }
    }

    Ok(())
}
