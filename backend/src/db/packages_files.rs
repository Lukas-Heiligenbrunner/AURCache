//! `SeaORM` Entity. Generated by sea-orm-codegen 0.11.2

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq)]
#[sea_orm(table_name = "packages_files")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    pub file_id: i32,
    pub package_id: i32,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::packages::Entity",
        from = "Column::PackageId",
        to = "super::packages::Column::Id"
    )]
    Packages,
    #[sea_orm(
        belongs_to = "super::files::Entity",
        from = "Column::FileId",
        to = "super::files::Column::Id"
    )]
    Files,
}

impl ActiveModelBehavior for ActiveModel {}