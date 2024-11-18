use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::{env, fs};

pub fn create_build_paths(name: String) -> anyhow::Result<(String, PathBuf)> {
    // create builds dir
    let mut host_build_path_base = env::current_dir()?;
    host_build_path_base.push("builds");
    fs::create_dir_all(host_build_path_base.clone())?;
    fs::set_permissions(host_build_path_base.clone(), Permissions::from_mode(0o777))?;

    // create current build dir
    let mut host_active_build_path = host_build_path_base.clone();
    host_active_build_path.push(name);
    fs::create_dir_all(host_active_build_path.clone())?;
    fs::set_permissions(
        host_active_build_path.clone(),
        Permissions::from_mode(0o777),
    )?;

    // use either docker volume or base dir as docker host mount path
    let host_build_path_docker =
        env::var("BUILD_ARTIFACT_DIR").unwrap_or(host_build_path_base.display().to_string());
    Ok((host_build_path_docker, host_active_build_path))
}
