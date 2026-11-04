use sea_orm::DbBackend;
use std::env;

#[must_use]
pub fn database_type() -> DbBackend {
    match env::var("DB_TYPE").as_deref() {
        Ok("POSTGRESQL") => DbBackend::Postgres,
        _ => DbBackend::Sqlite,
    }
}
