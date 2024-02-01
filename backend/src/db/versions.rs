//! `SeaORM` Entity. Generated by sea-orm-codegen 0.11.2

use rocket::serde::Serialize;
use rocket_okapi::okapi::schemars;
use rocket_okapi::JsonSchema;
use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq, Serialize, JsonSchema)]
#[sea_orm(table_name = "versions")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    pub version: String,
    pub package_id: i32,
    pub file_name: Option<String>,
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
        belongs_to = "super::packages::Entity",
        from = "Column::Id",
        to = "super::packages::Column::LatestVersionId"
    )]
    LatestPackage,
    #[sea_orm(has_many = "super::builds::Entity")]
    Builds,
}

// `Related` trait has to be implemented by hand
impl Related<super::packages::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Packages.def()
    }
}

// impl Related<super::builds::Entity> for Entity {
//     fn to() -> RelationDef {
//         Relation::Builds.def()
//     }
// }

impl ActiveModelBehavior for ActiveModel {}