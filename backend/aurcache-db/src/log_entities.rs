//! What each log entry refers to: the index behind "everything about X".
//!
//! One row per entity an entry names, derived from its payload at insert. The
//! reference is split into the namespace and the id it travels as
//! (`pkg:hello` -> `pkg`, `hello`), because that pair is what every query is
//! written against.
//!
//! `role` is the payload key the reference was found under, so a filter can ask
//! for one role specifically (`the dependent`) without reading JSON. A role
//! naming several entities has a row each, which is why it is part of the key
//! rather than unique on its own.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq)]
#[sea_orm(table_name = "log_entity")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub log_id: i32,
    /// The payload key this reference was found under: `old`, `dependent`,
    /// `builds`. `scope` for the entry's own scope.
    #[sea_orm(primary_key, auto_increment = false)]
    pub role: String,
    /// `pkg`, `worker` or `build`.
    #[sea_orm(primary_key, auto_increment = false)]
    pub ns: String,
    /// The identifier within that namespace: a pkgbase, a worker name, or
    /// `pkgbase/number`.
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::logs::Entity",
        from = "Column::LogId",
        to = "super::logs::Column::Id",
        on_delete = "Cascade"
    )]
    Log,
}

impl Related<super::logs::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Log.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
