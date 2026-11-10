use sea_orm::DbBackend;

#[must_use]
pub fn database_type() -> DbBackend {
    match aurcache_common::server_env::DB_TYPE.get().as_deref() {
        Ok("POSTGRESQL") => DbBackend::Postgres,
        _ => DbBackend::Sqlite,
    }
}
