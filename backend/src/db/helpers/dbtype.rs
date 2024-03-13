use std::env;

pub enum DbType {
    SQLITE,
    POSTGRES,
}

pub fn database_type() -> DbType {
    env::var("DB_TYPE").map_or(DbType::SQLITE, |t| {
        if t == "POSTGRESQL" {
            DbType::POSTGRES
        } else {
            DbType::SQLITE
        }
    })
}