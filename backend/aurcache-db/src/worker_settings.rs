//! `SeaORM` Entity for values set for a worker's settings on the server.
//!
//! Keyed by `(worker_id, key)`. The value is stored as the operator wrote it,
//! already checked against the worker's declaration; the server never reads it
//! as anything but a string, and the worker parses it on delivery.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq)]
#[sea_orm(table_name = "worker_settings")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    pub worker_id: i32,
    /// The key the worker declared the setting under, e.g. `concurrency`.
    pub key: String,
    pub value: String,
}

impl ActiveModelBehavior for ActiveModel {}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::workers::Entity",
        from = "Column::WorkerId",
        to = "super::workers::Column::Id"
    )]
    Workers,
}

impl Related<super::workers::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Workers.def()
    }
}
