//! 0.5.0's curated activity log, which the structured log replaced.
//!
//! Kept only so `aurcache_activitylog::legacy` can carry its rows across; it
//! empties itself as they move, and nothing writes here.

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
