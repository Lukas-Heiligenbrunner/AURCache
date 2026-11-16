//! The settings the Kubernetes worker declares, beside the protocol ones every
//! worker has.
//!
//! What a build may *reach* is not declared: the builds' namespace, the images,
//! the credential Secret, user namespaces and the pod template stay the
//! machine's. See `design/implemented/worker-configuration.md`.

use aurcache_worker_core::settings::SettingSpec;

/// Keys of the settings this worker accepts, matching the chroot worker's
/// where it has the same setting.
pub mod keys {
    pub const BUILD_MEMORY_MAX: &str = "build_memory_max";
    pub const BUILD_CPUS: &str = "build_cpus";
    pub const BUILD_DISK_MAX: &str = "build_disk_max";
    pub const SCHEDULE_TIMEOUT: &str = "schedule_timeout";
    pub const KEYSERVER: &str = "keyserver";
}

/// The settings this worker accepts, on top of the protocol ones.
#[must_use]
pub fn kube_settings() -> Vec<SettingSpec> {
    // WP4 writes this.
    unimplemented!()
}
