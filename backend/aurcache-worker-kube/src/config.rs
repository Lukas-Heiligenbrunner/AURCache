//! The dispatcher's configuration: its environment, and what each build
//! takes from the declared settings.
//!
//! The variables are in `kubernetes-executor-plan.md`, section 4.6.

use anyhow::Result;
use aurcache_worker_core::config::CoreConfig;
use aurcache_worker_core::settings::WorkerSettings;
use serde::Deserialize;
use std::time::Duration;

/// What every worker reads, advertising the dispatcher's own architecture
/// unless `WORKER_ARCHES` says otherwise.
#[must_use]
pub fn core_from_env() -> CoreConfig {
    // WP4 writes this.
    unimplemented!()
}

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub namespace: String,
    /// The label value every pod of this dispatcher carries: the first 16
    /// hex digits of its fingerprint.
    pub dispatcher: String,
    pub builder_image: String,
    pub pull_policy: String,
    pub agent_image: AgentImage,
    pub credential_secret: Option<String>,
    pub user_namespaces: bool,
    pub extras: PodExtras,
    pub network_probe: bool,
    pub tunables: Tunables,
}

/// Where the `stage` and `hold` containers' image comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentImage {
    /// Named by `WORKER_KUBE_AGENT_IMAGE`.
    Given(String),
    /// The image the dispatcher's own pod runs.
    OwnPod { namespace: String, name: String },
}

/// What each build takes from the declared settings.
#[derive(Clone, Debug, PartialEq)]
pub struct Tunables {
    pub memory: Option<u64>,
    pub cpus: Option<f64>,
    pub disk: Option<u64>,
    pub schedule_timeout: Duration,
    pub keyserver: String,
}

/// The fields of a build pod an operator may add, from the JSON file
/// `WORKER_KUBE_POD_TEMPLATE` names: scheduling, labels and annotations,
/// never a volume, a container or a security context. An unknown field is an
/// error.
// The fields are WP4's.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct PodExtras {}

impl Config {
    pub fn from_env(settings: &WorkerSettings, fingerprint: &str) -> Result<Self> {
        // WP4 writes this.
        let _ = (settings, fingerprint);
        unimplemented!()
    }

    /// This configuration with the tunables of a new resolution of the
    /// settings, for the next build.
    #[must_use]
    pub fn with_settings(&self, settings: &WorkerSettings) -> Self {
        // WP4 writes this.
        let _ = settings;
        unimplemented!()
    }
}
