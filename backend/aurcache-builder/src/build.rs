use crate::logger::BuildLogger;
use crate::path_utils::create_active_build_path;
use anyhow::{anyhow, bail};
use aurcache_db::dependencies;
use aurcache_db::helpers::active_value_ext::ActiveValueExt;
use aurcache_db::prelude::{Builds, Dependencies, Files, Packages};
use aurcache_db::{builds, files, packages};
use aurcache_types::builder::{Action, BuildStates};
use aurcache_types::settings::{ApplicationSettings, Setting, SettingSource, SettingsEntry};
use aurcache_utils::settings::general::SettingsTraits;
use aurcache_utils::utils::remove_archive_file::try_remove_archive_file;
use bollard::Docker;
use bollard::query_parameters::{
    KillContainerOptions, StartContainerOptions, WaitContainerOptions,
};
use futures::StreamExt;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, IntoActiveModel, Order,
    QueryFilter, QueryOrder, QuerySelect, Set, TransactionTrait, TryIntoModel,
};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;
use tokio::sync::broadcast::Sender;
use tokio::time::timeout;
use tracing::{debug, info};

struct BuildDirGuard {
    path: PathBuf,
    id: i32,
}

impl Drop for BuildDirGuard {
    fn drop(&mut self) {
        info!("Build {}: Remove shared build folder", self.id);
        let _ = fs::remove_dir_all(&self.path);
    }
}

pub struct Builder {
    pub(crate) db: DatabaseConnection,
    pub(crate) job_containers: Arc<Mutex<HashMap<i32, String>>>,
    pub(crate) package_model: packages::ActiveModel,
    pub(crate) build_model: builds::ActiveModel,
    pub(crate) logger: BuildLogger,
    pub(crate) docker: Docker,
    pub(crate) action_tx: Sender<Action>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DepState {
    Satisfied,
    NeedsRebuild,
    NotReady,
}

impl Builder {
    pub async fn new(
        db: DatabaseConnection,
        job_containers: Arc<Mutex<HashMap<i32, String>>>,
        package_model: packages::Model,
        build_model: builds::Model,
        action_tx: Sender<Action>,
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
            action_tx,
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

        if builder_image.source != SettingSource::Default {
            info!(
                "Build #{}: Builder Image resolved from {:?} to: {}",
                self.build_model.id.get()?,
                builder_image.source,
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
        let host_active_build_path = create_active_build_path(pkgname)?;

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
        // RAII guard: clean up build dir when Builder is dropped, regardless of success/failure
        let _guard = BuildDirGuard {
            path: host_active_build_path.clone(),
            id: *self.build_model.id.get()?,
        };

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
        let pkg_id = *self.package_model.id.get()?;
        let pkg_exists = Packages::find_by_id(pkg_id).one(&self.db).await?.is_some();

        let txn = self.db.begin().await?;
        self.build_model.end_time = Set(Some(
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64,
        ));

        match result {
            Ok(()) => {
                if pkg_exists {
                    self.package_model.status = Set(BuildStates::SUCCESSFUL_BUILD);
                    self.package_model.out_of_date = Set(i32::from(false));
                    if let Ok(version) = self.build_model.version.get() {
                        self.package_model.current_version = Set(Some(version.clone()));
                    }
                    self.package_model = self.package_model.clone().save(&txn).await?;
                }

                self.build_model.status = Set(Some(BuildStates::SUCCESSFUL_BUILD));

                self.build_model = self.build_model.clone().save(&txn).await?;
                txn.commit().await?;

                if pkg_exists {
                    self.logger
                        .append("finished package build".to_string())
                        .await;

                    if let Err(e) = self.trigger_dependents().await {
                        self.logger
                            .append(format!("Failed to trigger dependents: {e}"))
                            .await;
                    }
                } else {
                    self.logger
                        .append(
                            "package was removed during build; cleaning up artifacts".to_string(),
                        )
                        .await;
                    if let Err(e) = self.cleanup_orphaned_build_files().await {
                        self.logger
                            .append(format!("Failed to clean up orphaned files: {e}"))
                            .await;
                    }
                }
            }
            Err(e) => {
                if pkg_exists {
                    self.package_model.status = Set(BuildStates::FAILED_BUILD);
                    self.package_model = self.package_model.clone().save(&txn).await?;
                }

                self.build_model.status = Set(Some(BuildStates::FAILED_BUILD));
                self.build_model = self.build_model.clone().save(&txn).await?;
                txn.commit().await?;

                self.logger
                    .append("failed to build package".to_string())
                    .await;
                self.logger.append(e.to_string()).await;
            }
        }

        self.job_containers
            .lock()
            .await
            .remove(self.build_model.id.get()?)
            .ok_or(anyhow!("Failed to get job container"))?;
        Ok(())
    }

    /// Create a build record and send it to the build queue.
    async fn enqueue_build(
        &self,
        pkg: &packages::Model,
        platform: &str,
        version: &str,
    ) -> anyhow::Result<builds::Model> {
        let build = builds::ActiveModel {
            pkg_id: Set(pkg.id),
            output: Set(None),
            status: Set(Some(BuildStates::ENQUEUED_BUILD)),
            platform: Set(platform.to_string()),
            start_time: Set(Some(
                SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64,
            )),
            version: Set(version.to_string()),
            ..Default::default()
        };
        let saved = build.save(&self.db).await?;
        let model = saved.try_into_model()?;
        let _ = self.action_tx.send(Action::Build(
            Box::from(pkg.clone()),
            Box::new(model.clone()),
        ));
        Ok(model)
    }

    /// After a successful build, check for packages that depend on this one
    /// and trigger their builds if all their dependencies are satisfied.
    async fn trigger_dependents(&self) -> anyhow::Result<()> {
        let pkg_id = *self.package_model.id.get()?;

        let dep_links = Dependencies::find()
            .filter(dependencies::Column::DependeeId.eq(pkg_id))
            .all(&self.db)
            .await?;

        if dep_links.is_empty() {
            return Ok(());
        }

        let dependent_ids: Vec<i32> = dep_links.iter().map(|l| l.dependent_id).collect();
        let mut deps_by_dependent: HashMap<i32, Vec<dependencies::Model>> = HashMap::new();
        for dep in Dependencies::find()
            .filter(dependencies::Column::DependentId.is_in(dependent_ids))
            .all(&self.db)
            .await?
        {
            deps_by_dependent
                .entry(dep.dependent_id)
                .or_default()
                .push(dep);
        }

        for link in &dep_links {
            let dependent_id = link.dependent_id;
            let Some(all_deps) = deps_by_dependent.get(&dependent_id) else {
                continue;
            };

            let mut all_satisfied = true;
            for dep in all_deps {
                match self
                    .check_dep(dep.dependee_id, &dep.version_constraint)
                    .await?
                {
                    DepState::Satisfied => continue,
                    DepState::NeedsRebuild => {
                        self.trigger_dep_rebuild(dep.dependee_id).await?;
                        all_satisfied = false;
                        break;
                    }
                    DepState::NotReady => {
                        all_satisfied = false;
                        break;
                    }
                }
            }

            if all_satisfied
                && let Some(pkg) = Packages::find_by_id(dependent_id).one(&self.db).await?
                && pkg.status != BuildStates::SUCCESSFUL_BUILD
                && pkg.status != BuildStates::ACTIVE_BUILD
                && pkg.status != BuildStates::ENQUEUED_BUILD
            {
                let version = pkg
                    .current_version
                    .clone()
                    .or(pkg.upstream_version.clone())
                    .unwrap_or_default();

                let platform_strs: Vec<&str> = pkg.platforms.split(';').collect();
                for platform in platform_strs {
                    let new_build = self.enqueue_build(&pkg, platform, &version).await?;
                    self.logger
                        .append(format!(
                            "Triggered build #{} for dependent '{}'",
                            new_build.id, pkg.name
                        ))
                        .await;
                }
            }
        }
        Ok(())
    }

    /// Check if a dependency has a successful build at a version that satisfies
    /// the version constraint.
    async fn check_dep(&self, dependee_id: i32, constraint: &str) -> anyhow::Result<DepState> {
        // Self-referencing dep (shouldn't happen, but match original behavior)
        if *self.package_model.id.get()? == dependee_id {
            return Ok(DepState::Satisfied);
        }

        let Some(pkg) = Packages::find_by_id(dependee_id).one(&self.db).await? else {
            return Ok(DepState::NotReady);
        };

        // Already building or queued — not ready yet, don't trigger duplicate
        if pkg.status == BuildStates::ACTIVE_BUILD || pkg.status == BuildStates::ENQUEUED_BUILD {
            return Ok(DepState::NotReady);
        }

        // Get the latest successful build
        let Some(build) = Builds::find()
            .select_only()
            .column(builds::Column::Version)
            .column(builds::Column::Status)
            .filter(builds::Column::PkgId.eq(dependee_id))
            .filter(builds::Column::Status.eq(Some(BuildStates::SUCCESSFUL_BUILD)))
            .order_by(builds::Column::EndTime, Order::Desc)
            .order_by(builds::Column::StartTime, Order::Desc)
            .limit(1)
            .into_tuple::<(String, Option<i32>)>()
            .one(&self.db)
            .await?
        else {
            return Ok(DepState::NotReady);
        };

        let (version, _status) = build;
        if constraint.is_empty() || aurcache_utils::pkg::satisfies_constraint(&version, constraint)
        {
            Ok(DepState::Satisfied)
        } else {
            Ok(DepState::NeedsRebuild)
        }
    }

    /// Trigger a rebuild of a dependency whose version doesn't satisfy the constraint,
    /// unless it's already building.
    async fn trigger_dep_rebuild(&self, dependee_id: i32) -> anyhow::Result<()> {
        let Some(pkg) = Packages::find_by_id(dependee_id).one(&self.db).await? else {
            return Ok(());
        };

        // Already building or queued — no duplicate
        if pkg.status == BuildStates::ACTIVE_BUILD || pkg.status == BuildStates::ENQUEUED_BUILD {
            self.logger
                .append(format!(
                    "Dep '{}' is already building, skipping duplicate rebuild",
                    pkg.name
                ))
                .await;
            return Ok(());
        }

        let version = pkg
            .current_version
            .clone()
            .or(pkg.upstream_version.clone())
            .unwrap_or_default();

        let platform_strs: Vec<&str> = pkg.platforms.split(';').collect();
        for platform in platform_strs {
            let new_build = self.enqueue_build(&pkg, platform, &version).await?;
            self.logger
                .append(format!(
                    "Triggered rebuild #{} for dep '{}' (version constraint)",
                    new_build.id, pkg.name
                ))
                .await;
        }

        Ok(())
    }

    /// Clean up package files that were placed into the repo by `move_and_add_pkgs`
    /// when the package record was deleted during the build.
    async fn cleanup_orphaned_build_files(&self) -> anyhow::Result<()> {
        let pkg_id = *self.package_model.id.get()?;
        let platform = self.build_model.platform.get()?.clone();

        let orphaned: Vec<files::Model> = Files::find()
            .filter(files::Column::PackageId.eq(pkg_id))
            .filter(files::Column::Platform.eq(&platform))
            .all(&self.db)
            .await?;

        if orphaned.is_empty() {
            return Ok(());
        }

        let txn = self.db.begin().await?;
        for file in orphaned {
            self.logger
                .append(format!(
                    "Cleaning up orphaned build artifact: {}\n",
                    file.filename
                ))
                .await;
            try_remove_archive_file(file, &txn).await?;
        }
        txn.commit().await?;
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
