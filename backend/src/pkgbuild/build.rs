use anyhow::anyhow;
use std::fs;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader, Lines};
use tokio::sync::broadcast::Sender;

// todo consider removing pkg_vers from attribute list
pub async fn build_pkgbuild(
    folder_path: String,
    pkg_vers: &str,
    pkg_name: &str,
    tx: Sender<String>,
) -> anyhow::Result<Vec<String>> {
    // update pacman cache
    let mut child = tokio::process::Command::new("pacman")
        .args(["-Sy"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child.wait().await?;

    let makepkg = include_str!("../../scripts/makepkg");

    // Create a temporary file to store the bash script content
    let script_file = std::env::temp_dir().join("makepkg_custom.sh");
    fs::write(&script_file, makepkg).expect("Unable to write script to file");

    let mut child = tokio::process::Command::new("bash")
        .args(&[
            script_file.as_os_str().to_str().unwrap(),
            "-f",
            "--noconfirm",
            "--nocolor",
            "-s",              // install required deps
            "-c",              // cleanup leftover files and dirs
            "--rmdeps",        // remove installed deps with -s
            "--noprogressbar", // pacman shouldn't display a progressbar
        ])
        .current_dir(folder_path.clone())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let stderr = child
        .stderr
        .take()
        .ok_or(anyhow!("failed to take stderr"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or(anyhow!("failed to take stdout"))?;

    let stderr = BufReader::new(stderr).lines();
    let stdout = BufReader::new(stdout).lines();

    let tx1 = tx.clone();
    spawn_broadcast_sender(stderr, tx1);
    spawn_broadcast_sender(stdout, tx);

    let result = child.wait().await;

    match result {
        Ok(result) => {
            if !result.success() {
                return Err(anyhow!("failed to build package"));
            }
        }
        Err(err) => {
            eprintln!("Failed to execute makepkg: {}", err);
            return Err(anyhow!("failed to build package"));
        }
    }

    locate_built_packages(pkg_name.to_string(), folder_path)
}

fn spawn_broadcast_sender<R: tokio::io::AsyncRead + Unpin + Send + 'static>(
    mut reader: Lines<BufReader<R>>,
    tx: Sender<String>,
) {
    tokio::spawn(async move {
        while let Ok(Some(line)) = reader.next_line().await {
            // println!("directerr: {line}");
            let _ = tx.send(line);
        }
    });
}

/// a pkgbuild might build multiple packages
/// todo handle case later to pick only relevant one
fn locate_built_packages(pkg_name: String, folder_path: String) -> anyhow::Result<Vec<String>> {
    let mut pkg_names: Vec<String> = vec![];

    if let Ok(paths) = fs::read_dir(folder_path) {
        for path in paths {
            if let Ok(path) = path {
                let path = path.path();
                if let Some(file_name) = path.file_name() {
                    let file_name = file_name.to_str().unwrap();

                    if file_name.ends_with(".pkg.tar.zst") {
                        pkg_names.push(file_name.to_string());
                    }
                }
            }
        }
    }

    return if pkg_names.is_empty() {
        Err(anyhow!("Built package not found"))
    } else {
        // expect at least one of the packages to start with the package name
        if !pkg_names.iter().any(|x| x.starts_with(&pkg_name)) {
            return Err(anyhow!(
                "None of the built packages starts with the expected name"
            ));
        }
        Ok(pkg_names)
    };
}
