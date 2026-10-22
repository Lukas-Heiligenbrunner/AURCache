use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "settings")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    pub key: String,
    pub value: Option<String>,
    /// The package the value is for, or [`GLOBAL`]; see [`scope`] and
    /// [`package_of`].
    pub pkg_id: i32,
}

/// `pkg_id` standing for "applies to the whole server".
///
/// A sentinel rather than NULL because the column is NOT NULL, which the
/// `UNIQUE (pkg_id, key)` constraint depends on: NULLs do not compare equal, so
/// a nullable column would let the same global key be inserted twice. Only
/// [`scope`] and [`package_of`] spell it; everything else says `None`.
const GLOBAL: i32 = -1;

/// The `pkg_id` a value for `pkg_id` -- a package, or `None` for the whole
/// server -- is stored under.
#[must_use]
pub const fn scope(pkg_id: Option<i32>) -> i32 {
    match pkg_id {
        Some(id) => id,
        None => GLOBAL,
    }
}

/// The package a stored `pkg_id` is for; `None` for a server-wide value.
#[must_use]
pub const fn package_of(stored: i32) -> Option<i32> {
    if stored == GLOBAL { None } else { Some(stored) }
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
