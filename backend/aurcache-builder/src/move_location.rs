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

#[derive(Debug, Clone)]
struct ParsedPkg {
    name: String,
    #[allow(unused)]
    version: String,
    #[allow(unused)]
    arch: String,
    #[allow(unused)]
    filename: String,
}

impl Builder {
    pub(crate) async fn move_and_add_pkgs(&self, host_build_path: PathBuf) -> anyhow::Result<()> {
        let archive_paths = fs::read_dir(host_build_path.clone())?.collect::<Vec<_>>();
        if archive_paths.is_empty() {
            bail!("No files found in build directory");
        }

        let build_pkgs = build_output_map(archive_paths)?;
        let txn = self.db.begin().await?;
        let pkg_id = *self.package_model.id.get()?;
        let platform = self.build_model.platform.get()?.clone();

        let mut new_file_ids: HashMap<String, i32> = HashMap::new();

        for (archive_path, parsed) in &build_pkgs {
            let archive_name = archive_path.file_name().to_str().unwrap().to_string();
            let pkg_path = format!("./repo/{platform}/{archive_name}");

            let existing = Files::find()
                .filter(files::Column::Filename.eq(&archive_name))
                .filter(files::Column::Platform.eq(&platform))
                .one(&txn)
                .await?;

            let file_id = if let Some(existing) = existing {
                if existing.package_id != pkg_id {
                    // During dependency-resolution migration, files can move from a legacy owner
                    // package to the newly created dependency package that should own them.
                    let existing_owner_depends_on_new_owner = Dependencies::find()
                        .filter(dependencies::Column::DependentId.eq(existing.package_id))
                        .filter(dependencies::Column::DependeeId.eq(pkg_id))
                        .one(&txn)
                        .await?;

                    if existing_owner_depends_on_new_owner.is_none() {
                        bail!("File '{archive_name}' is already produced by another package");
                    }
                    self.logger
                        .append(format!(
                            "Transferring file '{archive_name}' from package {} (depends on this package)\n",
                            existing.package_id
                        ))
                        .await;
                }

                let mut active: files::ActiveModel = existing.into();
                active.package_id = Set(pkg_id);
                active.update(&txn).await?.id
            } else {
                files::ActiveModel {
                    filename: Set(archive_name.clone()),
                    platform: Set(platform.clone()),
                    package_id: Set(pkg_id),
                    ..Default::default()
                }
                .insert(&txn)
                .await?
                .id
            };
            new_file_ids.insert(parsed.name.clone(), file_id);

            self.logger
                .append(format!("Move {} to repo directory\n", parsed.filename))
                .await;
            fs::copy(archive_path.path(), &pkg_path)?;
            fs::remove_file(archive_path.path())?;

            self.logger
                .append(format!(
                    "Add {} to repo.db.tar.gz and repo.files.tar.gz\n",
                    parsed.filename
                ))
                .await;
            pacman_repo_utils::repo_add::repo_add(
                &pkg_path,
                format!("./repo/{platform}/repo.db.tar.gz"),
                format!("./repo/{platform}/repo.files.tar.gz"),
            )?;
        }

        let stale = Files::find()
            .filter(files::Column::PackageId.eq(pkg_id))
            .filter(files::Column::Platform.eq(&platform))
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
        self.logger
            .append("Successfully updated repo and cleaned up old files\n".to_string())
            .await;
        Ok(())
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
            continue;
        }

        let parsed = parse_arch_pkg(name)?;
        map.push((a, parsed));
    }

    Ok(map)
}
