//! What every container worker does with a claimed job before it builds:
//! offer it the SSH key, and download its source.

use crate::ssh::{self, Credential, CredentialFiles};
use anyhow::{Context, Result};
use aurcache_build_agent::spec::SshAccess;
use aurcache_worker_core::client::WorkerClient;
use aurcache_worker_core::executor::ClaimedJob;
use aurcache_worker_core::protocol;
use aurcache_worker_core::ssh_key;

/// A job ready to build.
pub struct Prepared {
    /// The source archive, as downloaded.
    pub source: Vec<u8>,
    /// The key in an ssh-agent of this build's own, when the build is
    /// offered it. The agent is killed when this is dropped.
    pub credential: Option<Credential>,
}

impl Prepared {
    /// The hosts the build may use the key with, for its spec.
    #[must_use]
    pub fn ssh_access(&self) -> Option<SshAccess> {
        self.credential.as_ref().map(|credential| SshAccess {
            known_hosts: credential.known_hosts.clone(),
        })
    }
}

/// Prepare `claimed` on a worker that holds `key`, or no key.
///
/// Whether the key is offered is said first in the build log, so a fetch that
/// fails for want of it has the reason above it. Such a worker holds no named
/// key, so none is ever withheld. The ssh-agent is started from the files as
/// they are now, so that a changed key or host keys are used from the next
/// build on.
pub async fn prepare(
    client: &WorkerClient,
    claimed: &ClaimedJob,
    key: Option<&CredentialFiles>,
) -> Result<Prepared> {
    let build_id = claimed.job.build_id;
    let held = ssh::held(key);
    let offer = ssh_key::offer(&claimed.job, &held, true);
    if let Some(notice) = offer.notice(&claimed.job.pkgbase) {
        protocol::log(client, build_id, &format!("[worker] {notice}\n")).await;
    }
    let source = client
        .source(build_id)
        .await
        .context("downloading source")?;
    let credential = match offer.key() {
        Some(files) => Some(files.load().await.context("the worker's SSH key")?),
        None => None,
    };
    Ok(Prepared { source, credential })
}
