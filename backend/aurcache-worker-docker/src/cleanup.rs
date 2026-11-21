//! Removing what a build leaves: its container, and its directory even when
//! root in the build left some of it beyond this worker's own user.

use anyhow::{Context, Result};
use aurcache_worker_container::arch::Platform;
use bollard::Docker;
use bollard::query_parameters::{
    CreateContainerOptions, RemoveContainerOptions, StartContainerOptions, WaitContainerOptions,
};
use futures::StreamExt;

use crate::config::BuildDirs;
use crate::container;
use crate::workdir::{self, Removal};

/// A build directory to remove, and what to remove it with when this
/// worker's own user cannot.
pub struct Leftover<'a> {
    pub dirs: &'a BuildDirs,
    /// The image the cleaning container is made from: the builder image,
    /// which is already there.
    pub image: &'a str,
    pub platform: Platform,
    /// The cleaning container's name.
    pub name: String,
}

/// Remove a container, whatever state it is in; nothing, when there is none.
pub async fn remove_container(docker: &Docker, name_or_id: &str) {
    let _ = docker
        .remove_container(
            name_or_id,
            Some(RemoveContainerOptions {
                force: true,
                ..Default::default()
            }),
        )
        .await;
}

/// Remove a build's directory: as this worker's user, and when some of it is
/// left, after a container of the builder image has emptied it as root.
pub async fn clear(docker: &Docker, leftover: &Leftover<'_>) -> Result<()> {
    if let Removal::Removed = workdir::remove(&leftover.dirs.local) {
        return Ok(());
    }
    remove_container(docker, &leftover.name).await;
    let emptied = empty(docker, leftover).await;
    remove_container(docker, &leftover.name).await;
    emptied?;
    match workdir::remove(&leftover.dirs.local) {
        Removal::Removed => Ok(()),
        Removal::Left(e) => Err(e).context("it is still there after emptying it as root"),
    }
}

/// Run the cleaning container to its end.
async fn empty(docker: &Docker, leftover: &Leftover<'_>) -> Result<()> {
    docker
        .create_container(
            Some(CreateContainerOptions {
                name: Some(leftover.name.clone()),
                platform: container::platform_name(leftover.platform),
            }),
            container::cleaner(leftover.image, &leftover.dirs.host),
        )
        .await
        .context("creating the cleaning container")?;
    docker
        .start_container(&leftover.name, None::<StartContainerOptions>)
        .await
        .context("starting the cleaning container")?;
    let mut wait = docker.wait_container(&leftover.name, None::<WaitContainerOptions>);
    match wait.next().await {
        Some(Ok(_)) | None => Ok(()),
        Some(Err(e)) => Err(e).context("emptying the build directory as root"),
    }
}
