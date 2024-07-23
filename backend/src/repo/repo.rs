use anyhow::anyhow;
use std::process::Command;

static REPO_NAME: &str = "repo";

pub fn repo_add(pkg_file_name: String) -> anyhow::Result<()> {
    let db_file = format!("{REPO_NAME}.db.tar.gz");

    let output = Command::new("repo-add")
        .args(&[db_file.clone(), pkg_file_name, "--nocolor".to_string()])
        .current_dir("./repo/")
        .output()?;

    if !output.status.success() {
        return Err(anyhow!(
            "Error exit code when repo-add: {}{}",
            String::from_utf8_lossy(output.stdout.as_slice()),
            String::from_utf8_lossy(output.stderr.as_slice())
        ));
    }

    println!("{db_file} updated successfully");
    Ok(())
}

fn repo_remove(pkg_file_name: String) -> anyhow::Result<()> {
    let db_file = format!("{REPO_NAME}.db.tar.gz");

    let output = Command::new("repo-remove")
        .args(&[db_file.clone(), pkg_file_name, "--nocolor".to_string()])
        .current_dir("./repo/")
        .output()?;

    if !output.status.success() {
        return Err(anyhow!(
            "Error exit code when repo-remove: {}{}",
            String::from_utf8_lossy(output.stdout.as_slice()),
            String::from_utf8_lossy(output.stderr.as_slice())
        ));
    }

    println!("{db_file} updated successfully");
    Ok(())
}
