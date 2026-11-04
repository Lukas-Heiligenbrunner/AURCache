//! A long-running operation and its progress.
//!
//! Bulk adds and restores both take minutes and both have to report what they
//! are doing to an observer that may not be there. See
//! `helpers::operations` for the reading and writing, and the migration for
//! why the two share a table.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq)]
#[sea_orm(table_name = "operations")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    /// What kind of operation this is, so one kind's rows can be told from
    /// another's. See [`crate::helpers::operations`] for the values.
    pub kind: String,
    pub created_at: i64,
    /// `None` while the job is still running.
    pub finished_at: Option<i64>,
    /// How many items the request carried, fixed when the operation is created.
    pub total: i32,
    pub completed: i32,
    pub failed: i32,
    /// Append-only, one JSON outcome per line, read by line offset. The entry
    /// shape is the caller's -- each kind of operation reports its own.
    pub log: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
