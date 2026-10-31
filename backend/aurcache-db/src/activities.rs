//! The curated activity log, which the structured log replaced.
//!
//! Kept only so `aurcache_activitylog::legacy` can carry its rows across; it
//! empties itself as they move, and nothing writes here any more.

use sea_orm::entity::prelude::*;

#[derive(Debug, Clone, Copy, PartialEq, EnumIter, DeriveActiveEnum, Eq)]
#[sea_orm(rs_type = "i32", db_type = "Integer")]
pub enum ActivityType {
    #[sea_orm(num_value = 0)]
    AddPackage,
    #[sea_orm(num_value = 1)]
    RemovePackage,
    #[sea_orm(num_value = 2)]
    UpdatePackage,
    #[sea_orm(num_value = 3)]
    StartBuild,
    #[sea_orm(num_value = 4)]
    FinishBuild,
    /// The server process started. A deploy, a restart, or a crash loop -- all
    /// three are worth being able to line up against what else happened.
    #[sea_orm(num_value = 5)]
    ServerStart,
    /// A machine asked to join the fleet for the first time.
    #[sea_orm(num_value = 6)]
    WorkerEnroll,
    #[sea_orm(num_value = 7)]
    WorkerApprove,
    #[sea_orm(num_value = 8)]
    WorkerRevoke,
    /// A build produced a package that never reached the repository.
    #[sea_orm(num_value = 9)]
    PublishFailed,
    /// A worker stopped answering and the builds it held were requeued.
    #[sea_orm(num_value = 10)]
    WorkerReaped,
    /// A pass of the version check did not finish, so nothing was found to be
    /// out of date that pass.
    #[sea_orm(num_value = 11)]
    VersionCheckFailed,
    /// A worker refused a value its machine was configured with, and is running
    /// something else.
    #[sea_orm(num_value = 12)]
    WorkerSettingRejected,
}

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq)]
#[sea_orm(table_name = "activity")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    pub typ: ActivityType,
    pub data: String, // json object
    pub timestamp: i64,
    pub user: Option<String>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for crate::activities::ActiveModel {}
