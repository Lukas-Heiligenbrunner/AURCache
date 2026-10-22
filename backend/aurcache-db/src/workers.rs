//! `SeaORM` Entity for remote build workers.

use crate::lists::WorkerList;
use aurcache_common::api::worker::ApprovalStatus;
use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq)]
#[sea_orm(table_name = "workers")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    pub name: String,
    /// Where this worker is in the approval workflow.
    ///
    /// Typed rather than a string: it is read in a dozen places, and every one
    /// of them comparing against its own spelling is how the three constants
    /// ended up defined in three crates.
    pub status: ApprovalStatus,
    /// SHA-256 fingerprint of the worker's self-generated certificate/CSR.
    /// Stable identity used for enrollment and mTLS mapping.
    pub cert_fingerprint: String,
    /// PEM of the CA-signed leaf certificate (set on approval).
    pub signed_cert: Option<String>,
    /// Epoch seconds when the signed certificate expires.
    pub not_after: Option<i64>,
    /// Architectures the worker builds natively.
    pub native_arches: WorkerList,
    /// Architectures the worker can build via emulation.
    pub emulated_arches: WorkerList,
    /// Epoch seconds of the last heartbeat/contact.
    pub last_seen: Option<i64>,
    /// Worker software version reported at enrollment/heartbeat.
    pub version: Option<String>,
    /// Which build strategy the worker runs -- `chroot`, `docker`. Free-form,
    /// so a new executor needs no migration. `None` for a worker that enrolled
    /// before the column existed.
    pub kind: Option<String>,
    /// Exact pkgbase names this worker is specially provisioned
    /// for (credentials, licensed toolchain, scratch space). A package named by
    /// *any* approved worker may only be built by workers that name it.
    pub package_affinity: WorkerList,
    /// Scheduling preference; higher wins. A worker declines a job only while a
    /// *strictly* higher-priority worker could take it, so the default of 0
    /// means nothing is ever held back.
    pub priority: i32,
    /// Maximum concurrent builds the worker reported at registration, used to
    /// decide whether it still has capacity.
    pub concurrency: i32,
    /// JSON array of the settings this worker declared it accepts
    /// (`aurcache_common::worker_config::SettingDecl`), replaced at every
    /// registration.
    ///
    /// A string rather than a richer type for the same reason `source_data` is
    /// one: the server stores and forwards it without interpreting it.
    pub settings_declaration: String,
    /// JSON of what those settings resolved to on the worker
    /// (`aurcache_common::worker_config::EffectiveConfig`), as last reported
    /// over the heartbeat. `None` until a worker has reported one.
    pub effective_config: Option<String>,
    /// An operator asked this worker to take no new builds and let the ones it
    /// holds finish, so the machine can be rebooted, upgraded or retired
    /// without cutting a build short. Unlike revoking, the worker stays
    /// trusted and keeps its builds; unlike a setting, it needs nothing from
    /// the worker -- the claim query simply stops offering it jobs.
    pub paused: bool,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
