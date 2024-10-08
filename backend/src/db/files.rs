//! `SeaORM` Entity. Generated by sea-orm-codegen 0.11.2

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq)]
#[sea_orm(table_name = "files")]
pub struct Model {
    pub filename: String,
    #[sea_orm(primary_key)]
    pub id: i32,
    pub platform: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

impl Related<super::packages::Entity> for Entity {
    fn to() -> RelationDef {
        super::packages_files::Relation::Packages.def()
    }

    fn via() -> Option<RelationDef> {
        Some(super::packages_files::Relation::Files.def().rev())
    }
}
