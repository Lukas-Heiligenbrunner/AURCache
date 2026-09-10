use crate::build::Builder;
use anyhow::{anyhow, bail};
use aurcache_db::dependencies;
use aurcache_db::files;
use aurcache_db::helpers::active_value_ext::ActiveValueExt;
use aurcache_db::prelude::{Dependencies, Files};
use aurcache_utils::utils::remove_archive_file::try_remove_archive_file;
use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, Set, TransactionTrait};
use std::collections::HashMap;
use std::fs;
use std::fs::DirEntry;
use std::path::PathBuf;

// todo this pkg file structure might be migrated to the sql database in the future
//  if it is used more often than here once
#[derive(Debug, Clone)]
struct ParsedPkg {
    name: String,
    version: String,
    #[allow(unused)]
    arch: String,
    #[allow(unused)]
    filename: String,
}

impl Builder {
    /// Move built files from the build container to the host and add them to the repo.
    ///
    /// Returns the version string extracted from the built package filenames (e.g. `"1.2.3-1"`).
    /// This is the version *actually produced by makepkg* and may differ from the version stored
    /// in `builds.version` at enqueue time (which comes from the AUR RPC).
    pub(crate) async fn move_and_add_pkgs(
        &self,
        host_build_path: PathBuf,
    ) -> anyhow::Result<String> {
        let archive_paths = fs::read_dir(host_build_path.clone())?.collect::<Vec<_>>();
        if archive_paths.is_empty() {
            bail!("No files found in build directory");
        }

        let build_pkgs = build_output_map(archive_paths)?;
        let pkg_id = *self.package_model.id.get()?;
        let platform = self.build_model.platform.get()?;

        // Extract the version from the first built package.  All split packages of the same
        // pkgbase share the same pkgver-pkgrel, so any one of them is representative.
        let actual_version = build_pkgs
            .first()
            .map(|(_, parsed)| parsed.version.clone())
            .ok_or_else(|| anyhow!("No packages found in build output"))?;

        // PHASE 1: resolve file ownership in a short read transaction so we
        // don't hold a DB connection during the long file-copy / repo_add phase.
        struct FileInfo {
            archive_name: String,
            pkg_path: String,
            parsed_name: String,
            existing_id: Option<i32>,
            existing_package_id: Option<i32>,
        }

        let mut file_infos: Vec<FileInfo> = Vec::new();
        {
            let txn = self.db.begin().await?;
            for (archive_path, parsed) in &build_pkgs {
                let archive_name = archive_path.file_name().to_str().unwrap().to_string();
                let pkg_path = format!("./repo/{platform}/{archive_name}");

                let existing = Files::find()
                    .filter(files::Column::Filename.eq(&archive_name))
                    .filter(files::Column::Platform.eq(*platform))
                    .one(&txn)
                    .await?;

                if let Some(ref ex) = existing
                    && ex.package_id != pkg_id
                {
                    let existing_owner_depends_on_new_owner = Dependencies::find()
                        .filter(dependencies::Column::DependentId.eq(ex.package_id))
                        .filter(dependencies::Column::DependeeId.eq(pkg_id))
                        .one(&txn)
                        .await?;

                    if existing_owner_depends_on_new_owner.is_none() {
                        bail!("File '{archive_name}' is already produced by another package");
                    }
                    self.logger
                            .append(format!(
                                "Transferring file '{archive_name}' from package {} (depends on this package)\n",
                                ex.package_id
                            ))
                            .await;
                }

                file_infos.push(FileInfo {
                    archive_name,
                    pkg_path,
                    parsed_name: parsed.name.clone(),
                    existing_id: existing.as_ref().map(|e| e.id),
                    existing_package_id: existing.as_ref().map(|e| e.package_id),
                });
            }
            txn.commit().await?;
        }

        // PHASE 2: copy files and update the pacman repo — no DB connection held.
        for fi in &file_infos {
            let archive_path = build_pkgs
                .iter()
                .find(|(de, _)| de.file_name().to_str().unwrap() == fi.archive_name)
                .map(|(de, _)| de.path())
                .expect("archive path must exist");

            self.logger
                .append(format!("Move {} to repo directory\n", fi.archive_name))
                .await;
            fs::copy(&archive_path, &fi.pkg_path)?;
            fs::remove_file(&archive_path)?;

            self.logger
                .append(format!(
                    "Add {} to repo.db.tar.gz and repo.files.tar.gz\n",
                    fi.archive_name
                ))
                .await;
            pacman_repo_utils::repo_add::repo_add(
                &fi.pkg_path,
                format!("./repo/{platform}/repo.db.tar.gz"),
                format!("./repo/{platform}/repo.files.tar.gz"),
            )?;
        }

        // PHASE 3: write the file records and remove stale entries in one short
        // transaction now that all filesystem work is done.
        let mut new_file_ids: HashMap<String, i32> = HashMap::new();
        {
            let txn = self.db.begin().await?;

            for fi in &file_infos {
                let file_id = if let Some(existing_id) = fi.existing_id {
                    if fi.existing_package_id != Some(pkg_id) {
                        // Transfer ownership to the current package.
                        let active = files::ActiveModel {
                            id: Set(existing_id),
                            package_id: Set(pkg_id),
                            ..Default::default()
                        };
                        active.update(&txn).await?.id
                    } else {
                        existing_id
                    }
                } else {
                    files::ActiveModel {
                        filename: Set(fi.archive_name.clone()),
                        platform: Set(*platform),
                        package_id: Set(pkg_id),
                        ..Default::default()
                    }
                    .insert(&txn)
                    .await?
                    .id
                };
                new_file_ids.insert(fi.parsed_name.clone(), file_id);
            }

            let stale = Files::find()
                .filter(files::Column::PackageId.eq(pkg_id))
                .filter(files::Column::Platform.eq(*platform))
                .all(&txn)
                .await?;

            for file in stale {
                if !new_file_ids.values().any(|&id| id == file.id) {
                    self.logger
                        .append(format!("Removing dropped sub-package: {}\n", file.filename))
                        .await;
                    try_remove_archive_file(file, &txn).await?;
                }
            }

            txn.commit().await?;
        }

        self.logger
            .append("Successfully updated repo and cleaned up old files\n".to_string())
            .await;
        Ok(actual_version)
    }
}

fn parse_arch_pkg(filename: &str) -> anyhow::Result<ParsedPkg> {
    let base = filename
        .split(".pkg.")
        .next()
        .ok_or_else(|| anyhow!("Invalid pkg filename: {filename}"))?;

    let parts: Vec<&str> = base.split('-').collect();
    if parts.len() < 4 {
        bail!("Invalid pkg filename format: {filename}");
    }

    let arch = parts[parts.len() - 1].to_string();
    let pkgrel = parts[parts.len() - 2];
    let pkgver = parts[parts.len() - 3];
    let name = parts[..parts.len() - 3].join("-");

    Ok(ParsedPkg {
        name,
        version: format!("{pkgver}-{pkgrel}"),
        arch,
        filename: filename.to_string(),
    })
}

fn build_output_map(
    archives: Vec<std::io::Result<DirEntry>>,
) -> anyhow::Result<Vec<(DirEntry, ParsedPkg)>> {
    let mut map = vec![];

    for a in archives {
        let a = a?;
        if a.file_type()?.is_dir() {
            continue;
        }
        let name = a.file_name();
        let name = name.to_str().ok_or_else(|| anyhow!("Invalid filename"))?;

        if name.starts_with('.') {
            // Produced packages are never hidden. We keep dotfiles in the build
            // dir for helper mounts such as temporary pacman configuration.
            continue;
        }

        let parsed = parse_arch_pkg(name)?;
        map.push((a, parsed));
    }

    Ok(map)
}
