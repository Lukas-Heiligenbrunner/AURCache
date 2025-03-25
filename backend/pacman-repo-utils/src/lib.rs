mod pkginfo;
mod repo_add;
mod repo_database;
mod repo_init;
mod repo_remove;

use crate::repo_add::repo_add_impl;
use crate::repo_init::init_repo_impl;
use crate::repo_remove::repo_remove_impl;
use std::path::PathBuf;

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

pub fn init_repo(path: &PathBuf, name: &str) -> anyhow::Result<()> {
    init_repo_impl(path, name)
}
