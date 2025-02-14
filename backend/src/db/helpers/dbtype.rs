use sea_orm::DbBackend;
use std::env;

pub fn database_type() -> DbBackend {
    env::var("DB_TYPE").map_or(DbBackend::Sqlite, |t| {
        if t == "POSTGRESQL" {
            DbBackend::Postgres
        } else {
            DbBackend::Sqlite
        }
    })
}
