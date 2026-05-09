use sea_orm::entity::prelude::*;
use serde::Serialize;
use utoipa::ToSchema;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq, Serialize, ToSchema)]
#[sea_orm(table_name = "dependencies")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    pub dependent_id: i32,
    pub dependee_id: i32,
    pub platforms: String,
    pub version_constraint: String,
}

impl ActiveModelBehavior for ActiveModel {}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::packages::Entity",
        from = "Column::DependentId",
        to = "super::packages::Column::Id"
    )]
    Dependent,
    #[sea_orm(
        belongs_to = "super::packages::Entity",
        from = "Column::DependeeId",
        to = "super::packages::Column::Id"
    )]
    Dependee,
}

impl Related<super::packages::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Dependee.def()
    }
}
