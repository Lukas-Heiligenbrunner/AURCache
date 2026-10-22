//! `SeaORM` entities for the structured operation log.
//!
//! See `design/implemented/structured-logs.md`; the schema is created by
//! `migration::m20261008_000000_from_0_5`.

use aurcache_common::api::activity::Severity;
use sea_orm::entity::prelude::*;

/// One entry.
#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq)]
#[sea_orm(table_name = "log")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    /// Stable identity of what happened, `domain.verb_object`.
    ///
    /// Text rather than an enum: the catalogue lives in the code that emits,
    /// and a server reading a row written by a newer version should store and
    /// show it rather than fail to parse the listing.
    pub kind: String,
    /// The severity that kind carries, stored as the number its variants are
    /// ordered by so "this and worse" is one comparison.
    pub severity: Severity,
    /// The sentence, rendered when the entry was written.
    ///
    /// Kept so a row still reads when `data` can no longer be parsed into the
    /// type it was written from -- the one failure the payload cannot survive
    /// on its own.
    pub message: String,
    /// The payload the message was rendered from, as JSON. Written and read
    /// whole; nothing queries into it.
    pub data: String,
    /// The entity this entry was emitted *under*, if any: `build:hello/7` for
    /// everything logged during that build. Also indexed in `log_entity`, so
    /// one query finds both the entries about an entity and those under it.
    pub scope: Option<String>,
    /// Unix seconds.
    pub timestamp: i64,
    /// `None` for anything the server did on its own, as opposed to a person
    /// asking for it.
    pub user: Option<String>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(has_many = "super::log_entities::Entity")]
    Entities,
}

impl Related<super::log_entities::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Entities.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
