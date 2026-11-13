//! The settings the legacy container executor declares, beside the protocol
//! ones every worker has.
//!
//! Its own, in its own units: `CPU_LIMIT` in milli-CPUs and `MEMORY_LIMIT` in
//! MB, read under their pre-worker names with their pre-worker meanings. The
//! chroot executor's `build_cpus` and `build_memory_max` say the same kind of
//! thing differently, and the server does not translate one into the other --
//! each worker tells it what it accepts.
//!
//! What a build may *reach* is not declared. The builder image (`BUILDER_IMAGE`)
//! decides the user and the privileges a build runs with, and the build
//! directory (`BUILD_ARTIFACT_DIR`) is a host path bound into every container,
//! so both stay the machine's. See `design/implemented/worker-configuration.md`.

use aurcache_common::worker_config::{Applies, ValueKind};
use aurcache_worker_core::settings::{Builtin, SettingSpec};

/// Keys of the settings this executor accepts.
pub mod keys {
    pub const CPU_LIMIT: &str = "cpu_limit";
    pub const MEMORY_LIMIT: &str = "memory_limit";
}

/// Unlimited, as the pre-worker default was.
pub const DEFAULT_CPU_LIMIT: i64 = 0;
/// Unlimited, as the pre-worker default was: any negative value.
pub const DEFAULT_MEMORY_LIMIT: i64 = -1;

/// The settings this executor accepts, on top of the protocol ones.
#[must_use]
pub fn docker_settings() -> Vec<SettingSpec> {
    vec![
        SettingSpec {
            key: keys::CPU_LIMIT,
            env_var: "CPU_LIMIT",
            kind: ValueKind::Integer {
                min: Some(0),
                max: None,
            },
            description: "CPUs each build container may use, in thousandths of a CPU: 2000 is \
                          two CPUs. 0 is unlimited.",
            category: "Build limits",
            applies: Applies::NextJob,
            default: Builtin::Integer(DEFAULT_CPU_LIMIT),
        },
        SettingSpec {
            key: keys::MEMORY_LIMIT,
            env_var: "MEMORY_LIMIT",
            kind: ValueKind::Integer {
                min: None,
                max: None,
            },
            description: "Memory each build container may use, swap included, in MB. Negative \
                          is unlimited.",
            category: "Build limits",
            applies: Applies::NextJob,
            default: Builtin::Integer(DEFAULT_MEMORY_LIMIT),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn the_whole_table_checks_out() {
        let all: Vec<_> = aurcache_worker_core::settings::protocol_settings()
            .into_iter()
            .chain(docker_settings())
            .collect();
        aurcache_worker_core::settings::check_table(&all);
    }

    /// The security boundary, asserted rather than left to review: the image a
    /// build runs in and the host path bound into it decide what a build can
    /// reach, and a server that could name them could choose both.
    #[test]
    fn nothing_that_decides_what_a_build_may_reach_is_declared() {
        let declared: HashSet<_> = aurcache_worker_core::settings::protocol_settings()
            .into_iter()
            .chain(docker_settings())
            .map(|spec| spec.env_var)
            .collect();
        for forbidden in [
            "BUILDER_IMAGE",
            "BUILD_ARTIFACT_DIR",
            "BUILD_ARTIFACT_DIR_LOCAL",
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
