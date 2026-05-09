use crate::build::Builder;
use anyhow::{anyhow, bail};
use aurcache_db::dependencies;
use aurcache_db::files;
use aurcache_db::helpers::active_value_ext::ActiveValueExt;
use aurcache_db::prelude::{Dependencies, Files};
use aurcache_utils::utils::remove_archive_file::try_remove_archive_file;
use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, Set, TransactionTrait};
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

        let mut new_file_ids: std::collections::HashMap<String, i32> =
            std::collections::HashMap::new();

        for (archive_path, parsed) in &build_pkgs {
            let archive_name = archive_path.file_name().to_str().unwrap().to_string();
            let platform = self.build_model.platform.get()?.clone();
            let pkg_path = format!("./repo/{platform}/{archive_name}");

            // Check that no other package claims this file
            if let Some(existing) = Files::find()
                .filter(files::Column::Filename.eq(&archive_name))
                .one(&txn)
                .await?
            {
                if existing.package_id != pkg_id {
                    // The claimant package may have produced this file in an older build
                    // (e.g. via paru which built deps inline) before dependency tracking
                    // was introduced. If the claimant depends on us, transfer ownership.
                    let claimant_depends = Dependencies::find()
                        .filter(dependencies::Column::DependentId.eq(existing.package_id))
                        .filter(dependencies::Column::DependeeId.eq(pkg_id))
                        .one(&txn)
                        .await?;

                    if claimant_depends.is_some() {
                        self.logger
                            .append(format!(
                                "Transferring file '{archive_name}' from package {} (depends on this package)\n",
                                existing.package_id
                            ))
                            .await;
                    } else {
                        bail!(
                            "File '{archive_name}' is already produced by another package"
                        );
                    }
                }
            }

            self.logger
                .append(format!("Move {} to repo directory\n", parsed.filename))
                .await;

            fs::copy(archive_path.path(), &pkg_path)?;
            fs::remove_file(archive_path.path())?;

            let file = match Files::find()
                .filter(files::Column::Filename.eq(&archive_name))
                .one(&txn)
                .await?
            {
                None => {
                    let file = files::ActiveModel {
                        filename: Set(archive_name.clone()),
                        platform: Set(platform.clone()),
                        package_id: Set(pkg_id),
                        ..Default::default()
                    };
                    file.insert(&txn).await?
                }
                Some(file) => {
                    let mut active: files::ActiveModel = file.into();
                    active.package_id = Set(pkg_id);
                    active.update(&txn).await?
                }
            };

            let file_id = file.id;
            new_file_ids.insert(parsed.name.clone(), file_id);

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

        // Remove any files this package owned that were NOT produced by the current build
        let stale = Files::find()
            .filter(files::Column::PackageId.eq(pkg_id))
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
