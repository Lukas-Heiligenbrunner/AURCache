mod pkginfo;
mod repo_add;
mod repo_database;
mod repo_remove;
use crate::repo_add::repo_add_impl;
use crate::repo_remove::repo_remove_impl;

pub fn repo_add(pkgfile: &str, db_archive: String, files_archive: String) -> anyhow::Result<()> {
    repo_add_impl(pkgfile, db_archive, files_archive)
}

pub fn repo_remove(
    filename: String,
    db_archive: String,
    files_archive: String,
) -> anyhow::Result<()> {
    repo_remove_impl(filename, db_archive, files_archive)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repoadd() {
        repo_add(
            "/home/lukas/RustroverProjects/untitled/backend/repo/yay-12.3.5-1-x86_64.pkg.tar.zst",
            "test.db.tar.gz".to_string(),
            "test.files.tar.gz".to_string(),
        )
        .unwrap()
    }

    #[test]
    fn repodel() {
        repo_remove(
            "yay-12.3.5-1".to_string(),
            "test.db.tar.gz".to_string(),
            "test.files.tar.gz".to_string(),
        )
        .unwrap()
    }
}
