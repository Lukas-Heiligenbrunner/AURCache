use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::{env, fs};

pub fn create_active_build_path(pkg_name: &str) -> anyhow::Result<PathBuf> {
    // this path is hardcoded at /app/builds/<pkgname>
    let path = env::current_dir()?.join("builds").join(pkg_name);
    fs::create_dir_all(&path)?;
    fs::set_permissions(&path, Permissions::from_mode(0o777))?;

    Ok(path)
}
