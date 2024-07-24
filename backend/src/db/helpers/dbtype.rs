use std::env;

pub enum DbType {
    Sqlite,
    Postgres,
}

pub fn database_type() -> DbType {
    env::var("DB_TYPE").map_or(DbType::Sqlite, |t| {
        if t == "POSTGRESQL" {
            DbType::Postgres
        } else {
            DbType::Sqlite
        }
    })
}
