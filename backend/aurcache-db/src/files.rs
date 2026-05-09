use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq)]
#[sea_orm(table_name = "files")]
pub struct Model {
    pub filename: String,
    #[sea_orm(primary_key)]
    pub id: i32,
    pub platform: String,
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
}

impl ActiveModelBehavior for ActiveModel {}

impl Related<super::packages::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Packages.def()
    }
}
