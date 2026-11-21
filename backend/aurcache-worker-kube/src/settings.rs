//! The settings the Kubernetes worker declares, beside the protocol ones every
//! worker has.
//!
//! What a build may *reach* is not declared: the builds' namespace, the images,
//! the credential Secret, user namespaces and the pod template stay the
//! machine's. See `design/implemented/worker-configuration.md`.

use aurcache_common::worker_config::{Applies, ValueKind};
use aurcache_worker_core::settings::{Builtin, DEFAULT_KEYSERVER, SettingSpec};
use std::time::Duration;

/// Keys of the settings this worker accepts, matching the chroot worker's
/// where it has the same setting.
pub mod keys {
    pub const BUILD_MEMORY_MAX: &str = "build_memory_max";
    pub const BUILD_CPUS: &str = "build_cpus";
    pub const BUILD_DISK_MAX: &str = "build_disk_max";
    pub const SCHEDULE_TIMEOUT: &str = "schedule_timeout";
    pub const KEYSERVER: &str = "keyserver";
}

/// How long a build pod may stay unschedulable when nothing says otherwise.
pub const DEFAULT_SCHEDULE_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// The settings this worker accepts, on top of the protocol ones.
#[must_use]
pub fn kube_settings() -> Vec<SettingSpec> {
    vec![
        SettingSpec {
            key: keys::BUILD_MEMORY_MAX,
            env_var: "WORKER_BUILD_MEMORY_MAX",
            kind: ValueKind::Size,
            description: "Memory one build container may use, as its request and its limit. \
                          Unset is unlimited. A build told it has this much waits for a node \
                          with this much free.",
            category: "Build limits",
            applies: Applies::NextJob,
            default: Builtin::Unset,
        },
        SettingSpec {
            key: keys::BUILD_CPUS,
            env_var: "WORKER_BUILD_CPUS",
            kind: ValueKind::Float {
                min: Some(0.0),
                max: None,
            },
            description: "CPUs one build container may use, as its request and its limit, \
                          fractions allowed. Also sizes the build's parallel jobs. Unset, or 0, \
                          is unlimited.",
            category: "Build limits",
            applies: Applies::NextJob,
            default: Builtin::Unset,
        },
        SettingSpec {
            key: keys::BUILD_DISK_MAX,
            env_var: "WORKER_BUILD_DISK_MAX",
            kind: ValueKind::Size,
            description: "Disk one build may write: the container's own layer and its work \
                          volume. Enforced by evicting the pod some seconds after it is passed, \
                          so it is a bound and not a quota. Unset is unlimited.",
            category: "Build limits",
            applies: Applies::NextJob,
            default: Builtin::Unset,
        },
        SettingSpec {
            key: keys::SCHEDULE_TIMEOUT,
            env_var: "WORKER_KUBE_SCHEDULE_TIMEOUT",
            kind: ValueKind::Duration,
            description: "How long a build pod may wait for a node, and then how long it \
                          may take to start on it, before the build is handed back.",
            category: "Build limits",
            applies: Applies::NextJob,
            default: Builtin::Duration(DEFAULT_SCHEDULE_TIMEOUT),
        },
        SettingSpec {
            key: keys::KEYSERVER,
            env_var: "WORKER_KEYSERVER",
            kind: ValueKind::Text,
            description: "Keyserver used to fetch the keys a PKGBUILD names in validpgpkeys. \
                          Which keys are accepted is pinned by the PKGBUILD, not by this.",
            category: "Signatures",
            applies: Applies::NextJob,
            default: Builtin::Text(DEFAULT_KEYSERVER),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn all() -> Vec<SettingSpec> {
        aurcache_worker_core::settings::protocol_settings()
            .into_iter()
            .chain(kube_settings())
            .collect()
    }

    #[test]
    fn the_whole_table_checks_out() {
        aurcache_worker_core::settings::check_table(&all());
    }

    #[test]
    fn limits_are_unset_by_default() {
        for spec in kube_settings() {
            if matches!(
                spec.key,
                keys::BUILD_MEMORY_MAX | keys::BUILD_CPUS | keys::BUILD_DISK_MAX
            ) {
                assert_eq!(spec.default, Builtin::Unset, "{}", spec.key);
            }
        }
    }

    /// What a build may reach is the machine's: a server that could name the
    /// namespace, the images, the credential or the pod template could choose
    /// what a build runs with.
    #[test]
    fn nothing_that_decides_what_a_build_may_reach_is_declared() {
        let declared: HashSet<_> = all().into_iter().map(|spec| spec.env_var).collect();
        for forbidden in [
            "WORKER_KUBE_NAMESPACE",
            "WORKER_KUBE_BUILDER_IMAGE",
            "WORKER_KUBE_IMAGE_PULL_POLICY",
            "WORKER_KUBE_AGENT_IMAGE",
            "WORKER_GIT_SSH_KEY",
            "WORKER_SSH_KNOWN_HOSTS",
            "WORKER_KUBE_USER_NAMESPACES",
            "WORKER_KUBE_POD_TEMPLATE",
            "WORKER_DATA_DIR",
            "AURCACHE_URL",
            "AURCACHE_ENROLLMENT_TOKEN",
            "AURCACHE_ENROLLMENT_DIR",
            "AURCACHE_SERVER_CA_FINGERPRINT",
        ] {
            assert!(
                !declared.contains(forbidden),
                "{forbidden} must not be settable from the server"
            );
        }
    }
}
