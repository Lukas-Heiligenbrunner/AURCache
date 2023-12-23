use crate::aur::aur::download_pkgbuild;
use crate::pkgbuild::build::{build_pkgbuild, build_repo_packagename};
use anyhow::anyhow;
use std::fs;
use std::process::Command;

static REPO_NAME: &str = "repo";
static BASEURL: &str = "https://aur.archlinux.org";

pub async fn add_pkg(url: String, version: String, name: String) -> anyhow::Result<()> {
    let fname = download_pkgbuild(format!("{}{}", BASEURL, url).as_str(), "./builds").await?;
    let pkg_file_name =
        build_pkgbuild(format!("./builds/{fname}"), version.as_str(), name.as_str())?;

    // todo force overwrite if file already exists
    fs::copy(
        format!("./builds/{fname}/{pkg_file_name}"),
        format!("./repo/{pkg_file_name}"),
    )?;
    fs::remove_file(format!("./builds/{fname}/{pkg_file_name}"))?;

    repo_add(pkg_file_name)?;

    Ok(())
}

fn repo_add(pkg_file_name: String) -> anyhow::Result<()> {
    let db_file = format!("{REPO_NAME}.db.tar.gz");

    let output = Command::new("repo-add")
        .args(&[db_file.clone(), pkg_file_name])
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
        .args(&[db_file.clone(), pkg_file_name])
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

pub async fn remove_pkg(pkg_name: String, pkg_version: String) -> anyhow::Result<()> {
    fs::remove_dir_all(format!("./builds/{pkg_name}"))?;

    let filename = build_repo_packagename(pkg_name.clone(), pkg_version);
    fs::remove_file(format!("./repo/{filename}"))?;

    repo_remove(pkg_name)?;

    Ok(())
}
