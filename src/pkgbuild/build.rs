use anyhow::anyhow;
use std::fs;
use std::io::{BufRead, BufReader};
use std::process::Command;
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

    let mut output = Command::new("bash")
        .args(&[
            script_file.as_os_str().to_str().unwrap(),
            "-f",
            "--noconfirm",
            "-s",
            "-c",
        ])
        .current_dir(folder_path.clone())
        .spawn()
        .unwrap();

    if let Some(stdout) = output.stdout.take() {
        let reader = BufReader::new(stdout);

        // Iterate through each line of output
        for line in reader.lines() {
            if let Ok(line_content) = line {
                // Print the line to the terminal
                println!("{}", line_content);

                // todo store line to database for being fetchable from api
            }
        }
    }

    // Ensure the command completes
    let result = output.wait();

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

    locate_built_package(pkg_name.to_string(), pkg_vers.to_string(), folder_path)
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
