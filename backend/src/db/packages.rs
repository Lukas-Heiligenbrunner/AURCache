//! `SeaORM` Entity. Generated by sea-orm-codegen 0.11.2

use rocket::serde::Serialize;
use rocket_okapi::okapi::schemars;
use rocket_okapi::JsonSchema;
use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq, Serialize, JsonSchema)]
#[sea_orm(table_name = "packages")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    pub name: String,
    pub status: i32,
    pub out_of_date: i32,
    pub latest_version_id: Option<i32>,
    pub latest_aur_version: Option<String>,
}

impl ActiveModelBehavior for ActiveModel {}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(has_many = "super::versions::Entity")]
    Versions,
    #[sea_orm(has_many = "super::builds::Entity")]
    Builds,
    #[sea_orm(has_one = "super::versions::Entity")]
    LatestVersion,
}

impl Related<super::versions::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Versions.def()
    }
}

// impl Related<super::versions::Entity> for Entity {
//     fn to() -> RelationDef {
//         Relation::LatestVersion.def()
//     }
// }

impl Related<super::builds::Entity> for crate::db::versions::Entity {
    fn to() -> RelationDef {
        Relation::Builds.def()
    }
}
