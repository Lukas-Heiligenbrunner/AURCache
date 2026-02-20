use crate::build::Builder;
use crate::logger::BuildLogger;
use crate::utils::remove_archive_file::try_remove_archive_file;
use anyhow::{anyhow, bail};
use aurcache_db::helpers::active_value_ext::ActiveValueExt;
use aurcache_db::prelude::{Files, PackagesFiles};
use aurcache_db::{files, packages_files};
use sea_orm::ColumnTrait;
use sea_orm::ModelTrait;
use sea_orm::PaginatorTrait;
use sea_orm::QueryFilter;
use sea_orm::QuerySelect;
use sea_orm::{ActiveModelTrait, EntityTrait, JoinType, RelationTrait, Set, TransactionTrait};
use std::fs;
use std::fs::DirEntry;
use std::path::PathBuf;

// todo this pkg file structure might be migrated to the sql database in the future
//  if it is used more often than here once
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
    /// move built files from build container to host and add them to the repo
    pub(crate) async fn move_and_add_pkgs(&self, host_build_path: PathBuf) -> anyhow::Result<()> {
        let archive_paths = fs::read_dir(host_build_path.clone())?.collect::<Vec<_>>();
        if archive_paths.is_empty() {
            bail!("No files found in build directory");
        }

        let build_pkgs = build_output_map(archive_paths)?;
        let txn = self.db.begin().await?;

        // ADD NEW FILES FIRST
        let mut new_file_ids = std::collections::HashMap::new();

        for (archive_path, parsed) in &build_pkgs {
            let archive_name = archive_path.file_name().to_str().unwrap().to_string();

            let pkg_path = format!(
                "./repo/{}/{}",
                self.build_model.platform.get()?,
                archive_name
            );

            self.logger
                .append(format!("Move {} to repo directory\n", parsed.filename))
                .await;

            fs::copy(archive_path.path(), &pkg_path)?;
            fs::remove_file(archive_path.path())?;

            // reuse file if it already exists
            let file = match Files::find()
                .filter(files::Column::Filename.eq(archive_name.clone()))
                .one(&txn)
                .await?
            {
                None => {
                    let file = files::ActiveModel {
                        filename: Set(archive_name.clone()),
                        platform: Set(self.build_model.platform.get()?.clone()),
                        ..Default::default()
                    };
                    file.save(&txn).await?
                }
                Some(file) => files::ActiveModel::from(file),
            };

            PackagesFiles::insert(packages_files::ActiveModel {
                file_id: file.id.clone(),
                package_id: Set(*self.package_model.id.get()?),
                ..Default::default()
            })
            .exec(&txn)
            .await?;

            let new_file_id = file.id.clone().unwrap();
            new_file_ids.insert(parsed.name.clone(), new_file_id);

            self.logger
                .append(format!(
                    "Add {} to repo.db.tar.gz and repo.files.tar.gz\n",
                    parsed.filename
                ))
                .await;
            pacman_repo_utils::repo_add::repo_add(
                &pkg_path,
                format!("./repo/{}/repo.db.tar.gz", self.build_model.platform.get()?),
                format!(
                    "./repo/{}/repo.files.tar.gz",
                    self.build_model.platform.get()?
                ),
            )?;

            // handle other package depending on an older version of this new package
            let older_versions = Files::find()
                .filter(files::Column::Platform.eq(self.build_model.platform.get()?))
                .filter(files::Column::Filename.starts_with(format!("{}-", parsed.name)))
                .filter(files::Column::Id.ne(new_file_id))
                .all(&txn)
                .await?;
            for old_v in older_versions {
                if let Ok(old_p) = parse_arch_pkg(&old_v.filename)
                    && old_p.name == parsed.name
                {
                    // repoint OTHER packages to this new version
                    repoint_dependents(
                        &txn,
                        &self.logger,
                        old_v.id,
                        new_file_id,
                        *self.package_model.id.get()?,
                    )
                    .await?;

                    // remove the link between our current package and the OLD version
                    packages_files::Entity::delete_many()
                        .filter(packages_files::Column::PackageId.eq(*self.package_model.id.get()?))
                        .filter(packages_files::Column::FileId.eq(old_v.id))
                        .exec(&txn)
                        .await?;

                    // if no one else is using the old version, delete it from DB and Disk
                    let usage_count = PackagesFiles::find()
                        .filter(packages_files::Column::FileId.eq(old_v.id))
                        .count(&txn)
                        .await?;

                    if usage_count == 0 {
                        self.logger
                            .append(format!(
                                "Removing orphaned old version: {}\n",
                                old_v.filename
                            ))
                            .await;
                        try_remove_archive_file(old_v, &txn).await?;
                    }
                }
            }
        }

        // handle dropped subpackages
        // fetch old associations NOW to ensure we see what's left after the name-based cleanup
        let remaining_old_files: Vec<(packages_files::Model, Option<files::Model>)> =
            PackagesFiles::find()
                .filter(packages_files::Column::PackageId.eq(*self.package_model.id.get()?))
                .filter(files::Column::Platform.eq(self.build_model.platform.get()?))
                .join(JoinType::LeftJoin, packages_files::Relation::Files.def())
                .select_also(files::Entity)
                .all(&txn)
                .await?;

        for (pkg_file, file) in remaining_old_files {
            let Some(file) = file else { continue };

            // If this file was NOT one of the ones we just built, it means the
            // PKGBUILD no longer produces this sub-package.
            if !new_file_ids.values().any(|&id| id == file.id) {
                let dependents =
                    dependent_packages(&txn, file.id, *self.package_model.id.get()?).await?;

                if dependents.is_empty() {
                    self.logger
                        .append(format!("Removing dropped sub-package: {}\n", file.filename))
                        .await;
                    pkg_file.delete(&txn).await?;
                    try_remove_archive_file(file, &txn).await?;
                } else {
                    self.logger
                        .append(format!(
                            "Keeping dropped sub-package {} (other dependents exist)\n",
                            file.filename
                        ))
                        .await;
                    pkg_file.delete(&txn).await?;
                }
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
        let name = a.file_name();
        let name = name.to_str().ok_or_else(|| anyhow!("Invalid filename"))?;

        let parsed = parse_arch_pkg(name)?;
        map.push((a, parsed));
    }

    Ok(map)
}

async fn dependent_packages(
    txn: &sea_orm::DatabaseTransaction,
    file_id: i32,
    current_pkg_id: i32,
) -> anyhow::Result<Vec<packages_files::Model>> {
    Ok(PackagesFiles::find()
        .filter(packages_files::Column::FileId.eq(file_id))
        .filter(packages_files::Column::PackageId.ne(current_pkg_id))
        .all(txn)
        .await?)
}

async fn repoint_dependents(
    txn: &sea_orm::DatabaseTransaction,
    logger: &BuildLogger,
    old_file_id: i32,
    new_file_id: i32,
    current_pkg_id: i32,
) -> anyhow::Result<()> {
    let deps = dependent_packages(txn, old_file_id, current_pkg_id).await?;
    logger
        .append(format!(
            "Repointing {} dependents to updated package\n",
            deps.len()
        ))
        .await;

    for dep in deps {
        let mut active = packages_files::ActiveModel::from(dep);
        active.file_id = Set(new_file_id);
        active.update(txn).await?;
    }

    Ok(())
}
