use crate::env::job_timeout_from_env;
use crate::logger::BuildLogger;
use crate::path_utils::create_active_build_path;
use crate::types::BuildStates;
use anyhow::{anyhow, bail};
use aurcache_db::helpers::active_value_ext::ActiveValueExt;
use aurcache_db::{builds, packages};
use bollard::Docker;
use bollard::query_parameters::{
    KillContainerOptions, StartContainerOptions, WaitContainerOptions,
};
use futures::StreamExt;
use sea_orm::{ActiveModelTrait, DatabaseConnection, IntoActiveModel, Set, TransactionTrait};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use std::{env, fs};
use tokio::sync::Mutex;
use tokio::time::timeout;
use tracing::{debug, info};

static BUILDER_IMAGE_DEFAULT: &str = "ghcr.io/lukas-heiligenbrunner/aurcache-builder:latest";

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

        let builder_image = match env::var("BUILDER_IMAGE") {
            Ok(v) => {
                info!("Using non-default Builder image: {v}");
                v
            }
            Err(_) => BUILDER_IMAGE_DEFAULT.to_string(),
        };

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
        self.wait_container_exit(&id).await?;
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

    async fn wait_container_exit(&self, container_id: &str) -> anyhow::Result<()> {
        let build_result = timeout(
            job_timeout_from_env(),
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
}
