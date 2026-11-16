//! Whether the builds' NetworkPolicy is enforced, found out at startup.
//!
//! A pod with the build pods' labels, so the policy selects it, tries the
//! server's worker port and the cloud metadata address, and the repository
//! when there is one. The cluster's API cannot say whether its CNI enforces
//! policies; this asks the network instead.

use crate::cluster::Cluster;
use crate::config::Config;

/// What the probe tries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Targets {
    /// `host:port` of the server's worker port.
    pub worker: String,
    /// `host:port` of the repository, when there is one.
    pub repo: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProbeOutcome {
    /// What must be closed is closed.
    Enforced,
    /// These answered and should not have.
    Open { reachable: Vec<String> },
    /// The probe could not be run or read.
    Inconclusive { reason: String },
}

/// Run the probe pod, read what it found, and delete it.
pub async fn check<C: Cluster>(
    cluster: &C,
    cfg: &Config,
    agent_image: &str,
    targets: &Targets,
) -> ProbeOutcome {
    // WP12 writes this.
    let _ = (cluster, cfg, agent_image, targets);
    unimplemented!()
}
