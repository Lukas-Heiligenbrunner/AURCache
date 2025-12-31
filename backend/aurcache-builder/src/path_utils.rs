use crate::build_mode::{BuildMode, get_build_mode};
use std::fs;
use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

/// create the build directory of the package to newly build (as path view of aurcache)
pub fn create_active_build_path(pkg_name: String) -> anyhow::Result<PathBuf> {
    let path = match get_build_mode() {
        BuildMode::DinD(v) => {
            let mut build_path = PathBuf::from(v.build_path);
            build_path.push(pkg_name);
            build_path
        }
        BuildMode::Host(v) => {
            let mut build_path = PathBuf::from(v.build_artifact_dir_aurcache);
            build_path.push(pkg_name);
            build_path
        }
    };

    fs::create_dir_all(path.clone())?;
    fs::set_permissions(path.clone(), Permissions::from_mode(0o777))?;

    Ok(path)
}
