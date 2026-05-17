use std::fs;

use alpm_srcinfo::SourceInfoV1;
use aurcache_db::packages::GitSourceSpec;
use tempfile::tempdir;

use crate::git::checkout::checkout_git_source;

/// Load a git package's source info by checking out its configured ref.
pub fn load_git_sourceinfo(spec: &GitSourceSpec) -> anyhow::Result<SourceInfoV1> {
    let dir = tempdir()?;
    let repo_path = dir.path().join("repo");
    checkout_git_source(spec, repo_path.clone())?;
    let package_dir = repo_path.join(&spec.subfolder);
    let srcinfo_path = package_dir.join(".SRCINFO");
    let sourceinfo = if srcinfo_path.exists() {
        SourceInfoV1::from_string(&fs::read_to_string(srcinfo_path)?)?
    } else {
        SourceInfoV1::from_pkgbuild(package_dir.join("PKGBUILD").as_path())?
    };
    dir.close()?;
    Ok(sourceinfo)
}
