use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "settings")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    pub key: String,
    pub value: Option<String>,
    pub pkg_id: Option<i32>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "crate::packages::Entity",
        from = "Column::PkgId",
        to = "crate::packages::Column::Id",
        on_update = "NoAction",
        on_delete = "NoAction"
    )]
    Package,
}

impl ActiveModelBehavior for ActiveModel {}
