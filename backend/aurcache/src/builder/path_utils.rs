use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::{env, fs};

pub fn create_active_build_path(pkg_name: String) -> anyhow::Result<PathBuf> {
    // this path is hardcoded at /app/builds/<pkgname>
    let mut path = env::current_dir()?;
    path.push("builds");
    path.push(pkg_name);
    fs::create_dir_all(path.clone())?;
    fs::set_permissions(path.clone(), Permissions::from_mode(0o777))?;

    Ok(path)
}
