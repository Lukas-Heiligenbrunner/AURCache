//! Tracks the last-seen upstream commit for each VCS (`git+...`) source entry
//! of a package, so version checks can detect upstream moves even when the
//! AUR-published `pkgver` hasn't been bumped (common for `-git`/VCS packages).
//!
//! Keyed by `(package_id, source_url)` since a single package can have
//! multiple `source` entries (including per-architecture ones), any subset of
//! which may be VCS URLs; `source_url` is the raw `.SRCINFO` source string
//! (e.g. `mypkg::git+https://example.com/repo.git#branch=develop`), which
//! already uniquely identifies each source line within a package.
use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq)]
#[sea_orm(table_name = "package_vcs_sources")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    pub package_id: i32,
    /// Raw `.SRCINFO` source string, e.g.
    /// `mypkg::git+https://example.com/repo.git#branch=develop`.
    pub source_url: String,
    /// Last resolved upstream commit hash for this source.
    pub last_commit: String,
    pub updated_at: i64,
}

impl ActiveModelBehavior for ActiveModel {}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::packages::Entity",
        from = "Column::PackageId",
        to = "super::packages::Column::Id"
    )]
    Packages,
}

impl Related<super::packages::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Packages.def()
    }
}
