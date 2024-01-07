use anyhow::anyhow;
use std::fs;
use std::process::Stdio;
use std::time::SystemTime;
use rocket::form::validate::Contains;
use tokio::io::{AsyncBufReadExt, BufReader, Lines};
use tokio::sync::broadcast::Sender;
use crate::aur::pkgbuild::PkgBuild;

pub async fn build_pkgbuild(
    pkg_build: PkgBuild,
    folder_path: String,
    pkg_vers: &str,
    pkg_name: &str,
    tx: Sender<String>,
) -> anyhow::Result<Vec<String>> {
    let makepkg = include_str!("../../scripts/makepkg");

    // Create a temporary file to store the bash script content
    let script_file = std::env::temp_dir().join("makepkg_custom.sh");
    fs::write(&script_file, makepkg).expect("Unable to write script to file");

    let mut child = tokio::process::Command::new("bash")
        .args([
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

    let archives = pkg_build.pkgname.iter().map(|x| {
        locate_built_package(x.to_string(), pkg_vers.to_string(), folder_path.clone())
    }).into_iter().collect();
    return archives
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

fn locate_built_package(
    pkg_name: String,
    pkg_vers: String,
    folder_path: String,
) -> anyhow::Result<String> {
    // check if expected built dir exists
    let built_name = build_expected_repo_packagename(pkg_name.to_string(), pkg_vers.to_string());
    if fs::metadata(format!("{folder_path}/{built_name}")).is_ok() {
        println!("Built {built_name}");
        return Ok(built_name.to_string());
    }

    // the naming might not always contain the build version
    // eg. mesa-git  --> match pkgname and extension if multiple return latest
    if let Ok(paths) = fs::read_dir(folder_path) {
        let mut candidate_filename: Option<String> = None;
        let mut candidate_timestamp = SystemTime::UNIX_EPOCH;

        for path in paths {
            if let Ok(path) = path {
                let path = path.path();
                if let Some(file_name) = path.file_name() {
                    let file_name = file_name.to_str().unwrap();

                    if file_name.ends_with("-x86_64.pkg.tar.zst")
                        && file_name.starts_with(pkg_name.as_str())
                    {
                        if let Ok(metadata) = path.metadata() {
                            if let Ok(modified_time) = metadata.modified() {
                                // Update the candidate filename and timestamp if the current file is newer
                                if modified_time > candidate_timestamp {
                                    candidate_filename = Some(file_name.to_string());
                                    candidate_timestamp = modified_time;
                                }
                            }
                        }
                    }
                }
            }
        }

        if candidate_filename.is_some() {
            println!("Built {}", candidate_filename.clone().unwrap());
            return Ok(candidate_filename.unwrap());
        }
    }

    Err(anyhow!("Built package not found"))
}

/// don't trust this pkg name from existing
/// pkgbuild might build different version name
pub fn build_expected_repo_packagename(pkg_name: String, pkg_vers: String) -> String {
    format!("{pkg_name}-{pkg_vers}-x86_64.pkg.tar.zst")
}
