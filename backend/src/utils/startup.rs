use log::{debug, error, info};
use std::path::PathBuf;
use tokio::fs;

#[cfg(target_arch = "x86_64")]
use {
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

    for cs in CONTAINER_STORAGE_DIRS {
        if fs::remove_dir_all(cs).await.is_ok() {
            info!("Removed old container storage `{}`", cs);
        }
    }

    if let Err(e) = pacman_repo_utils::init_repo(&PathBuf::from("./repo"), "repo") {
        error!("Failed to initialize pacman repo: {:?}", e);
    }

    #[cfg(target_arch = "x86_64")]
    init_qemu_binfmt().await.unwrap();
}

/// This is required to initialize the binfmt configuration for QEMU on x86_64 correctly
/// arm64 is not supported by qemu binfmt
/// see https://stackoverflow.com/questions/75954301/using-sudo-in-podman-with-qemu-architecture-emulation-leads-to-sudo-effective-u
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
