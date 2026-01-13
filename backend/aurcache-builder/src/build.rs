use crate::logger::BuildLogger;
use crate::path_utils::create_active_build_path;
use anyhow::{anyhow, bail};
use aurcache_db::helpers::active_value_ext::ActiveValueExt;
use aurcache_db::prelude::{Files, PackagesFiles};
use aurcache_db::{builds, files, packages, packages_files};
use aurcache_types::builder::BuildStates;
use aurcache_types::settings::{ApplicationSettings, Setting, SettingsEntry};
use aurcache_utils::settings::general::SettingsTraits;
use aurcache_utils::utils::remove_archive_file::try_remove_archive_file;
use bollard::Docker;
use bollard::query_parameters::{
    KillContainerOptions, StartContainerOptions, WaitContainerOptions,
};
use futures::StreamExt;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, IntoActiveModel, JoinType,
    ModelTrait, QueryFilter, QuerySelect, RelationTrait, Set, TransactionTrait,
};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;
use tokio::time::timeout;
use tracing::{debug, info};

pub struct Builder {
    pub(crate) db: DatabaseConnection,
    pub(crate) job_containers: Arc<Mutex<HashMap<i32, String>>>,
    pub(crate) package_model: packages::ActiveModel,
    pub(crate) build_model: builds::ActiveModel,
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
            Err(e) => {
                bail!("{e}");
            }
        };

        Ok(Builder {
            db,
            job_containers,
            package_model: package_model.into_active_model(),
            build_model: build_model.into_active_model(),
            logger,
            docker,
        })
    }

    pub async fn build(&mut self) -> anyhow::Result<()> {
        debug!(model = ?self.build_model);
        info!("Preparing build #{}", self.build_model.id.get()?);
        let target_platform = self.prepare_build().await?;

        let builder_image: SettingsEntry<String> = ApplicationSettings::get(
            Setting::BuilderImage,
            Some(*self.package_model.id.get()?),
            &self.db,
        )
        .await;

        if !builder_image.default {
            info!(
                "Build #{}: Builder Image overwritten by user to: {}",
                self.build_model.id.get()?,
                builder_image.value
            );
        }
        let builder_image = builder_image.value;

        info!(
            "Build #{}: Repull builder image",
            self.build_model.id.get()?
        );
        self.repull_image(builder_image.as_str(), target_platform.clone())
            .await?;

        info!(
            "Build #{}: Creating build container",
            self.build_model.id.get()?
        );

        let pkgname = self.package_model.name.get()?;
        let host_active_build_path = create_active_build_path(pkgname.clone())?;

        let create_info = self
            .create_build_container(target_platform, builder_image.as_str())
            .await?;
        let id = create_info.id;
        debug!(
            "Build #{}: build container created with id: {}",
            self.build_model.id.get()?,
            id
        );

        let docker2 = self.docker.clone();
        let id2 = id.clone();
        let build_logger2 = self.logger.clone();
        // start listening to container before starting it
        tokio::spawn(async move {
            _ = Self::monitor_build_output(&build_logger2, &docker2, id2).await;
        });

        // start build container
        info!(
            "Build #{}: starting build container",
            self.build_model.id.get()?
        );
        self.docker
            .start_container(&id, None::<StartContainerOptions>)
            .await?;

        // insert container id to container map
        self.job_containers
            .lock()
            .await
            .insert(*self.build_model.id.get()?, id.clone());

        // monitor build output
        debug!(
            "Build #{}: awaiting build container to exit",
            self.build_model.id.get()?
        );

        let job_timeout: u64 = ApplicationSettings::get(
            Setting::JobTimeout,
            Some(*self.package_model.id.get()?),
            &self.db,
        )
        .await
        .value;
        let job_timeout = Duration::from_secs(job_timeout);
        debug!("job_timeout: {} sec", job_timeout.as_secs());
        self.wait_container_exit(&id, job_timeout).await?;
        info!("Build #{id}: docker container exited successfully");

        // move built tar.gz archives to host and repo-add
        info!(
            "Build {}: Move built packages to repo",
            self.build_model.id.get()?
        );
        self.move_and_add_pkgs(host_active_build_path.clone())
            .await?;
        // remove active build dir
        info!(
            "Build {}: Remove shared build folder",
            self.build_model.id.get()?
        );
        fs::remove_dir(host_active_build_path)?;
        Ok(())
    }

    async fn wait_container_exit(
        &self,
        container_id: &str,
        job_timeout: Duration,
    ) -> anyhow::Result<()> {
        let build_result = timeout(
            job_timeout,
            self.docker
                .wait_container(
                    container_id,
                    Some(WaitContainerOptions {
                        condition: "not-running".to_string(),
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
                debug!("Build container exited with code: {exit_code}");
                if exit_code > 0 {
                    self.logger
                        .append(format!(
                            "Build #{} failed for package '{:?}', exit code: {}",
                            self.build_model.id.get()?,
                            self.package_model.name,
                            exit_code
                        ))
                        .await;
                    bail!("Build failed with exit code: {exit_code}");
                }
                Ok(())
            }
            // timeout branch
            Err(_) => {
                self.logger
                    .append(format!(
                        "Build #{} timed out for package '{:?}'",
                        self.build_model.id.get()?,
                        self.package_model.name
                    ))
                    .await;
                // kill build container
                self.docker
                    .kill_container(
                        container_id,
                        Some(KillContainerOptions {
                            signal: "SIGKILL".to_string(),
                        }),
                    )
                    .await?;
                bail!("Build timed out")
            }
        }
    }

    pub async fn post_build(&mut self, result: anyhow::Result<()>) -> anyhow::Result<()> {
        let txn = self.db.begin().await?;
        self.build_model.end_time = Set(Some(
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64,
        ));

        match result {
            Ok(()) => {
                // update package success status
                self.package_model.status = Set(BuildStates::SUCCESSFUL_BUILD);
                self.package_model.out_of_date = Set(i32::from(false));
                self.package_model = self.package_model.clone().save(&txn).await?;

                self.build_model.status = Set(Some(BuildStates::SUCCESSFUL_BUILD));

                self.build_model = self.build_model.clone().save(&txn).await?;
                // commit transaction before build logger requires db connection again
                txn.commit().await?;

                self.logger
                    .append("finished package build".to_string())
                    .await;
            }
            Err(e) => {
                self.package_model.status = Set(BuildStates::FAILED_BUILD);
                self.package_model = self.package_model.clone().save(&txn).await?;

                self.build_model.status = Set(Some(BuildStates::FAILED_BUILD));
                self.build_model = self.build_model.clone().save(&txn).await?;
                txn.commit().await?;

                self.logger
                    .append("failed to build package".to_string())
                    .await;
                self.logger.append(e.to_string()).await;
            }
        }

        // remove build from container map
        self.job_containers
            .lock()
            .await
            .remove(self.build_model.id.get()?)
            .ok_or(anyhow!("Failed to get job container"))?;
        Ok(())
    }

    pub async fn prepare_build(&mut self) -> anyhow::Result<String> {
        // set build status to building
        self.build_model.status = Set(Some(BuildStates::ACTIVE_BUILD));
        self.build_model.start_time = Set(Some(
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64,
        ));
        self.build_model = self.build_model.clone().save(&self.db).await?;

        // update status to building
        self.package_model.status = Set(BuildStates::ACTIVE_BUILD);
        self.package_model = self.package_model.clone().save(&self.db).await?;

        let target_platform = format!("linux/{}", self.build_model.platform.get()?);
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
            .filter(packages_files::Column::PackageId.eq(*self.package_model.id.get()?))
            .filter(files::Column::Platform.eq(self.build_model.platform.get()?))
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
            let pkg_path = format!(
                "./repo/{}/{}",
                self.build_model.platform.get()?,
                archive_name
            );
            // copy archive to repo, overwrite if file with same name exists
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
                        platform: Set(self.build_model.platform.get()?.clone()),
                        ..Default::default()
                    };
                    file.save(&txn).await?
                }
                Some(file) => files::ActiveModel::from(file),
            };

            let package_file = packages_files::ActiveModel {
                file_id: file.id,
                package_id: Set(*self.package_model.id.get()?),
                ..Default::default()
            };
            package_file.save(&txn).await?;

            pacman_repo_utils::repo_add::repo_add(
                pkg_path.as_str(),
                format!("./repo/{}/repo.db.tar.gz", self.build_model.platform.get()?),
                format!(
                    "./repo/{}/repo.files.tar.gz",
                    self.build_model.platform.get()?
                ),
            )?;
            info!("Successfully added '{archive_name}' to the repo archive");
        }
        txn.commit().await?;
        Ok(())
    }
}
