use crate::logger::BuildLogger;
use crate::path_utils::create_active_build_path;
use anyhow::{anyhow, bail};
use aurcache_db::dependencies;
use aurcache_db::helpers::active_value_ext::ActiveValueExt;
use aurcache_db::helpers::build_enqueue::promote_waiting_build;
use aurcache_db::prelude::{Builds, Packages};
use aurcache_db::{builds, packages};
use aurcache_types::builder::{Action, BuildStates};
use aurcache_types::settings::{ApplicationSettings, Setting, SettingSource, SettingsEntry};

use aurcache_deps::AurClient;
use aurcache_utils::settings::general::SettingsTraits;
use aurcache_utils::snapshot::SnapshotStore;
use bollard::Docker;
use bollard::query_parameters::{
    KillContainerOptions, StartContainerOptions, WaitContainerOptions,
};
use futures::StreamExt;
use pacman_mirrors::platforms::Platform;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, IntoActiveModel, Order,
    QueryFilter, QueryOrder, QuerySelect, Set, TransactionTrait,
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

/// RAII guard that removes the shared build directory on drop.
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

/// Orchestrates a single build from container creation through post-build cleanup.
pub struct Builder {
    pub(crate) db: DatabaseConnection,
    pub(crate) job_containers: Arc<Mutex<HashMap<i32, String>>>,
    pub(crate) package_model: packages::ActiveModel,
    pub(crate) build_model: builds::ActiveModel,
    pub(crate) logger: BuildLogger,
    pub(crate) docker: Docker,
    pub(crate) action_tx: Sender<Action>,
    pub(crate) client: AurClient,
    pub(crate) store: SnapshotStore,
}

impl Builder {
    /// Create a new Builder, establishing a Docker connection and initialising the build logger.
    pub async fn new(
        db: DatabaseConnection,
        job_containers: Arc<Mutex<HashMap<i32, String>>>,
        package_model: packages::Model,
        build_model: builds::Model,
        action_tx: Sender<Action>,
        client: AurClient,
        store: SnapshotStore,
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
            client,
            store,
        })
    }

    /// Run the full build lifecycle: prepare, containerise, wait, and move artefacts.
    pub async fn build(&mut self) -> anyhow::Result<()> {
        debug!(model = ?self.build_model);
        info!("Preparing build #{}", self.build_model.id.get()?);
        self.prepare_build().await?;

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
        let docker_platform = format!("linux/{}", self.build_model.platform.get()?);
        self.repull_image(builder_image.as_str(), docker_platform.clone())
            .await?;

        info!(
            "Build #{}: Creating build container",
            self.build_model.id.get()?
        );

        let pkgname = self.package_model.name.get()?;
        let host_active_build_path = create_active_build_path(pkgname)?;

        let create_info = self
            .create_build_container(docker_platform, builder_image.as_str())
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

        // Bail before touching the repo if the package was deleted during the build.
        let pkg_id = *self.package_model.id.get()?;
        if Packages::find_by_id(pkg_id).one(&self.db).await?.is_none() {
            bail!("package was removed during build; skipping repo update");
        }

        // move built tar.gz archives to host and repo-add, and retrieve the version
        // that makepkg actually built (may differ from the version recorded at enqueue time).
        info!(
            "Build {}: Move built packages to repo",
            self.build_model.id.get()?
        );
        let actual_version = self
            .move_and_add_pkgs(host_active_build_path.clone())
            .await?;

        // Reconcile the recorded version with what was actually built.
        let expected_version = self.build_model.version.get()?.clone();
        if actual_version != expected_version {
            self.logger
                .append(format!(
                    "Warning: actual built version '{actual_version}' differs from expected \
                     '{expected_version}'; updating build and package records\n"
                ))
                .await;
            info!(
                "Build {}: version mismatch — expected '{expected_version}', got '{actual_version}'",
                self.build_model.id.get()?
            );
        }
        // Always update to the version extracted from the package files: this is authoritative.
        self.build_model.version = Set(actual_version.clone());
        self.package_model.upstream_version = Set(Some(actual_version));

        Ok(())
    }

    /// Wait for the build container to exit, handling timeouts and non-zero exit codes.
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

    /// Record the build outcome in the database, trigger dependents on success.
    pub async fn post_build(&mut self, result: anyhow::Result<()>) -> anyhow::Result<()> {
        let txn = self.db.begin().await?;
        self.build_model.end_time = Set(Some(
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64,
        ));

        match result {
            Ok(()) => {
                self.package_model.status = Set(BuildStates::SUCCESSFUL_BUILD);
                self.package_model.out_of_date = Set(i32::from(false));
                self.package_model = self.package_model.clone().save(&txn).await?;
                self.build_model.status = Set(Some(BuildStates::SUCCESSFUL_BUILD));
                self.build_model = self.build_model.clone().save(&txn).await?;
                txn.commit().await?;
                self.logger
                    .append("finished package build".to_string())
                    .await;
                if let Err(e) = self.trigger_dependents().await {
                    self.logger
                        .append(format!("Failed to trigger dependents: {e}"))
                        .await;
                }
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
                tracing::error!("Build #{} failed: {e}", self.build_model.id.get()?);
            }
        }

        // Remove from the active container map. The container may be absent if
        // build() failed before the container was started — that is not an error.
        self.job_containers
            .lock()
            .await
            .remove(self.build_model.id.get()?);
        Ok(())
    }

    /// After a successful build, check for packages that depend on this one
    /// and trigger their builds if all their dependencies are satisfied.
    async fn trigger_dependents(&self) -> anyhow::Result<()> {
        let pkg_id = *self.package_model.id.get()?;
        let platform = *self.build_model.platform.get()?;
        let deps_by_dependent = self.load_dependencies_for_dependents_of(pkg_id).await?;
        if deps_by_dependent.is_empty() {
            return Ok(());
        }

        for (dependent_id, all_deps) in &deps_by_dependent {
            if self
                .dependencies_ready_for_dependent(all_deps, platform)
                .await?
            {
                self.trigger_dependent_builds(*dependent_id, platform)
                    .await?;
            }
        }
        Ok(())
    }

    /// Load all dependencies of every package that depends on the given package.
    async fn load_dependencies_for_dependents_of(
        &self,
        pkg_id: i32,
    ) -> anyhow::Result<HashMap<i32, Vec<dependencies::Model>>> {
        use aurcache_db::prelude::Dependencies;

        let dependent_ids: Vec<i32> = Dependencies::find()
            .filter(dependencies::Column::DependeeId.eq(pkg_id))
            .select_only()
            .column(dependencies::Column::DependentId)
            .into_tuple()
            .all(&self.db)
            .await?;

        if dependent_ids.is_empty() {
            return Ok(HashMap::new());
        }

        let deps = Dependencies::find()
            .filter(dependencies::Column::DependentId.is_in(dependent_ids))
            .all(&self.db)
            .await?;

        let mut deps_by_dependent: HashMap<i32, Vec<dependencies::Model>> = HashMap::new();
        for dep in deps {
            deps_by_dependent
                .entry(dep.dependent_id)
                .or_default()
                .push(dep);
        }
        Ok(deps_by_dependent)
    }

    /// Check whether all dependencies of a dependent are satisfied (or will be soon).
    async fn dependencies_ready_for_dependent(
        &self,
        all_deps: &[dependencies::Model],
        platform: Platform,
    ) -> anyhow::Result<bool> {
        for dep in all_deps {
            if !self
                .is_dependency_ready(dep.dependee_id, platform, &dep.version_constraint)
                .await?
            {
                return Ok(false);
            }
        }

        Ok(true)
    }

    /// Promote a dependent's waiting build to enqueued when all its deps are satisfied.
    async fn trigger_dependent_builds(
        &self,
        dependent_id: i32,
        platform: Platform,
    ) -> anyhow::Result<()> {
        let Some(pkg) = Packages::find_by_id(dependent_id).one(&self.db).await? else {
            // Dependent no longer exists - maybe was removed by the user in the meantime? In that
            // case nothing to do.
            return Ok(());
        };

        if !pkg.platforms.trim().is_empty()
            && !Platform::parse_many(&pkg.platforms).any(|r| r.is_ok_and(|p| p == platform))
        {
            // Dependent does not need the platform we just built. Nothing to do.
            return Ok(());
        }

        // Only promote builds that are already in the WAITING_FOR_DEPS state.
        let Some(promoted) = promote_waiting_build(&self.db, pkg.id, platform).await? else {
            // We never asked for this package to be (re)built. Nothing to do.
            return Ok(());
        };

        self.logger
            .append(format!(
                "Promoted build #{} for dependent '{}' on {} from waiting to enqueued",
                promoted.id, pkg.name, platform
            ))
            .await;

        let _ = self
            .action_tx
            .send(Action::Build(Box::from(pkg), Box::new(promoted)));
        Ok(())
    }

    /// Check if a dependency has a successful build at a version that satisfies
    /// the version constraint.
    async fn is_dependency_ready(
        &self,
        dependee_id: i32,
        platform: Platform,
        constraint: &str,
    ) -> anyhow::Result<bool> {
        // If a previous successful build already satisfies the constraint, we're
        // done regardless of any in-progress build.
        let latest_success = Builds::find()
            .select_only()
            .column(builds::Column::Version)
            .filter(builds::Column::PkgId.eq(dependee_id))
            .filter(builds::Column::Platform.eq(platform))
            .filter(builds::Column::Status.eq(Some(BuildStates::SUCCESSFUL_BUILD)))
            .order_by(builds::Column::EndTime, Order::Desc)
            .limit(1)
            .into_tuple::<String>()
            .one(&self.db)
            .await?;

        let Some(version) = latest_success else {
            return Ok(false);
        };

        Ok(aurcache_utils::pkg::satisfies_constraint(
            &version, constraint,
        ))
    }

    /// Mark the build as active in the database.
    pub async fn prepare_build(&mut self) -> anyhow::Result<()> {
        // set build status to building
        self.build_model.status = Set(Some(BuildStates::ACTIVE_BUILD));
        self.build_model.start_time = Set(Some(
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64,
        ));
        self.build_model = self.build_model.clone().save(&self.db).await?;

        // update status to building
        self.package_model.status = Set(BuildStates::ACTIVE_BUILD);
        self.package_model = self.package_model.clone().save(&self.db).await?;

        Ok(())
    }
}
