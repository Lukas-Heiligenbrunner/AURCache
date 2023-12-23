use anyhow::anyhow;
use std::fs;
use std::process::{Command, Stdio};
use std::time::SystemTime;

pub fn build_pkgbuild(
    folder_path: String,
    pkg_vers: &str,
    pkg_name: &str,
) -> anyhow::Result<String> {
    let makepkg = include_str!("../../scripts/makepkg");

    // Create a temporary file to store the bash script content
    let script_file = std::env::temp_dir().join("makepkg_custom.sh");
    fs::write(&script_file, makepkg).expect("Unable to write script to file");

    let output = Command::new("bash")
        .args(&[
            script_file.as_os_str().to_str().unwrap(),
            "-f",
            "--noconfirm",
            "-s",
            "-c",
        ])
        .current_dir(folder_path.clone())
        .stdout(Stdio::inherit())
        .spawn()
        .unwrap();
    let output = output.wait_with_output();

    match output {
        Ok(output) => {
            if output.status.success() {
                let stdout = String::from_utf8_lossy(&output.stdout);
                println!("makepkg output: {}", stdout);
            } else {
                let stderr = String::from_utf8_lossy(&output.stderr);
                eprintln!("makepkg error: {}", stderr);

                return Err(anyhow!("failed to build package"));
            }
        }
        Err(err) => {
            eprintln!("Failed to execute makepkg: {}", err);
            return Err(anyhow!("failed to build package"));
        }
    }

    // check if expected built dir exists
    let built_name = build_repo_packagename(pkg_name.to_string(), pkg_vers.to_string());
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

                    if file_name.ends_with("-x86_64.pkg.tar.zst") && file_name.starts_with(pkg_name)
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

    Err(anyhow!("No package built"))
}

pub fn build_repo_packagename(pkg_name: String, pkg_vers: String) -> String {
    format!("{pkg_name}-{pkg_vers}-x86_64.pkg.tar.zst")
}
