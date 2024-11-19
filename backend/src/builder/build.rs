use crate::builder::env::job_timeout_from_env;
use crate::builder::logger::BuildLogger;
use crate::builder::types::BuildStates;
use crate::db::files::ActiveModel;
use crate::db::migration::JoinType;
use crate::db::prelude::{Files, PackagesFiles};
use crate::db::{builds, files, packages, packages_files};
use crate::repo::utils::try_remove_archive_file;
use anyhow::{anyhow, bail};
use bollard::container::{KillContainerOptions, WaitContainerOptions};
use bollard::Docker;
use log::{debug, info};
use rocket::futures::StreamExt;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, IntoActiveModel, ModelTrait,
    QueryFilter, QuerySelect, RelationTrait, Set, TransactionTrait, TryIntoModel,
};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;
use tokio::time::timeout;

static BUILDER_IMAGE: &str = "ghcr.io/lukas-heiligenbrunner/aurcache-builder:latest";

pub struct Builder {
    pub(crate) db: DatabaseConnection,
    pub(crate) job_containers: Arc<Mutex<HashMap<i32, String>>>,
    pub(crate) package_model: packages::Model,
    pub(crate) build_model: builds::Model,
    pub(crate) logger: BuildLogger,
    pub(crate) docker: Docker,
}

impl Builder {
    pub async fn new(
        db: DatabaseConnection,
        job_containers: Arc<Mutex<HashMap<i32, String>>>,
        package_model: packages::Model,
        build_model: builds::Model,
    ) -> anyhow::Result<Self> {
        let logger = BuildLogger::new(build_model.id, db.clone());
        debug!("Build {}: Establish docker connection", build_model.id);
        let docker = Self::establish_docker_connection().await;

        let docker = match docker {
            Ok(docker) => docker,
            Err(_) => {
                bail!("Failed to establish docker connection");
            }
        };

        Ok(Builder {
            db,
            job_containers,
            package_model,
            build_model,
            logger,
            docker,
        })
    }

    pub async fn build(&mut self) -> anyhow::Result<()> {
        debug!("Preparing build #{}", self.build_model.id);
        let target_platform = self.prepare_build().await?;

        debug!("Build #{}: Repull builder image", self.build_model.id);
        let image_id = self
            .repull_image(BUILDER_IMAGE, target_platform.clone())
            .await?;
        debug!(
            "Build #{}: Image pulled with id: {}",
            self.build_model.id, image_id
        );

        debug!("Build #{}: Creating build container", self.build_model.id);
        let (create_info, host_active_build_path) = self
            .create_build_container(target_platform, image_id)
            .await?;
        let id = create_info.id;
        debug!(
            "Build #{}: build container created with id: {}",
            self.build_model.id, id
        );

        let docker2 = self.docker.clone();
        let id2 = id.clone();
        let build_logger2 = self.logger.clone();
        // start listening to container before starting it
        tokio::spawn(async move {
            _ = Self::monitor_build_output(&build_logger2, &docker2, id2.clone()).await;
        });

        // start build container
        debug!("Build #{}: starting build container", self.build_model.id);
        self.docker.start_container::<String>(&id, None).await?;

        // insert container id to container map
        self.job_containers
            .lock()
            .await
            .insert(self.build_model.id, id.clone());

        // monitor build output
        debug!(
            "Build #{}: awaiting build container to exit",
            self.build_model.id
        );
        let build_result = timeout(
            job_timeout_from_env(),
            self.docker
                .wait_container(
                    &id,
                    Some(WaitContainerOptions {
                        condition: "not-running",
                    }),
                )
                .next(),
        )
        .await;

        debug!("Build container was removed");

        match build_result {
            Ok(v) => {
                let t = v.ok_or(anyhow!("Failed to get build result"))??;
                let exit_code = t.status_code;
                if exit_code != 0 {
                    self.logger
                        .append(format!(
                            "Build #{} failed for package '{:?}', exit code: {}",
                            self.build_model.id, self.package_model.name, exit_code
                        ))
                        .await;
                    bail!("Build failed with exit code: {}", exit_code);
                }
            }
            // timeout branch
            Err(_) => {
                self.logger
                    .append(format!(
                        "Build #{} timed out for package '{:?}'",
                        self.build_model.id, self.package_model.name
                    ))
                    .await;
                // kill build container
                self.docker
                    .kill_container(&id, Some(KillContainerOptions { signal: "SIGKILL" }))
                    .await?;
            }
        }

        // move built tar.gz archives to host and repo-add
        debug!("Build {}: Move built packages to repo", self.build_model.id);
        self.move_and_add_pkgs(host_active_build_path.clone())
            .await?;
        // remove active build dir
        debug!("Build {}: Remove shared build folder", self.build_model.id);
        fs::remove_dir(host_active_build_path)?;
        Ok(())
    }

    pub async fn post_build(&mut self, result: anyhow::Result<()>) -> anyhow::Result<()> {
        let txn = self.db.begin().await?;
        self.build_model.end_time =
            Some(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64);

        match result {
            Ok(_) => {
                // update package success status
                self.package_model.status = BuildStates::SUCCESSFUL_BUILD;
                self.package_model.out_of_date = false as i32;
                self.package_model = self
                    .package_model
                    .clone()
                    .into_active_model()
                    .update(&txn)
                    .await?;

                self.build_model.status = Some(BuildStates::SUCCESSFUL_BUILD);

                self.build_model = self
                    .build_model
                    .clone()
                    .into_active_model()
                    .update(&txn)
                    .await?;
                // commit transaction before build logger requires db connection again
                txn.commit().await?;

                self.logger
                    .append("finished package build".to_string())
                    .await;
            }
            Err(e) => {
                self.package_model.status = BuildStates::FAILED_BUILD;
                self.package_model = self
                    .package_model
                    .clone()
                    .into_active_model()
                    .update(&txn)
                    .await?;

                self.build_model.status = Some(BuildStates::FAILED_BUILD);
                self.build_model = self
                    .build_model
                    .clone()
                    .into_active_model()
                    .update(&txn)
                    .await?;
                txn.commit().await?;

                self.logger.append(e.to_string()).await;
            }
        };

        // remove build from container map
        self.job_containers
            .lock()
            .await
            .remove(&self.build_model.id)
            .ok_or(anyhow!("Failed to get job container"))?;
        Ok(())
    }

    pub async fn prepare_build(&mut self) -> anyhow::Result<String> {
        // set build status to building
        self.build_model.status = Some(BuildStates::ACTIVE_BUILD);
        self.build_model.start_time =
            Some(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64);
        self.build_model = self
            .build_model
            .clone()
            .into_active_model()
            .save(&self.db)
            .await?
            .try_into_model()?;

        // update status to building
        self.package_model.status = BuildStates::ACTIVE_BUILD;
        self.package_model = self
            .package_model
            .clone()
            .into_active_model()
            .update(&self.db)
            .await?;

        let target_platform = format!("linux/{}", self.build_model.platform);

        #[cfg(target_arch = "aarch64")]
        if target_platform != "linux/arm64" {
            bail!("Unsupported host architecture aarch64 for cross-compile");
        }

        Ok(target_platform)
    }

    /// move built files from build container to host and add them to the repo
    async fn move_and_add_pkgs(&self, host_build_path: PathBuf) -> anyhow::Result<()> {
        let archive_paths = fs::read_dir(host_build_path.clone())?.collect::<Vec<_>>();
        if archive_paths.is_empty() {
            bail!("No files found in build directory");
        }

        // remove old files from repo and from direcotry
        // remove files assosicated with package
        let old_files: Vec<(packages_files::Model, Option<files::Model>)> = PackagesFiles::find()
            .filter(packages_files::Column::PackageId.eq(self.package_model.id))
            .filter(files::Column::Platform.eq(&self.build_model.platform))
            .join(JoinType::LeftJoin, packages_files::Relation::Files.def())
            .select_also(files::Entity)
            .all(&self.db)
            .await?;

        self.logger
            .append(format!(
                "Copy {} files from build dir to repo\nDeleting {} old files\n",
                archive_paths.len(),
                old_files.len()
            ))
            .await;

        let txn = self.db.begin().await?;
        for (pkg_file, file) in old_files {
            pkg_file.delete(&txn).await?;

            if let Some(file) = file {
                try_remove_archive_file(file, &txn).await?;
            }
        }

        for archive in archive_paths {
            let archive = archive?;
            let archive_name = archive
                .file_name()
                .to_str()
                .ok_or(anyhow!("Failed to get string from filename"))?
                .to_string();
            let pkg_path = format!("./repo/{}/{}", self.build_model.platform, archive_name);
            fs::copy(archive.path(), pkg_path.clone())?;
            // remove old file from shared path
            fs::remove_file(archive.path())?;

            let file = match Files::find()
                .filter(files::Column::Filename.eq(archive_name.clone()))
                .one(&txn)
                .await?
            {
                None => {
                    let file = files::ActiveModel {
                        filename: Set(archive_name.clone()),
                        platform: Set(self.build_model.platform.clone()),
                        ..Default::default()
                    };
                    file.save(&txn).await?
                }
                Some(file) => ActiveModel::from(file),
            };

            let package_file = packages_files::ActiveModel {
                file_id: file.id,
                package_id: Set(self.package_model.id),
                ..Default::default()
            };
            package_file.save(&txn).await?;

            pacman_repo_utils::repo_add(
                pkg_path.as_str(),
                format!("./repo/{}/repo.db.tar.gz", self.build_model.platform),
                format!("./repo/{}/repo.files.tar.gz", self.build_model.platform),
            )?;
            info!("Successfully added '{}' to the repo archive", archive_name);
        }
        txn.commit().await?;
        Ok(())
    }
}
