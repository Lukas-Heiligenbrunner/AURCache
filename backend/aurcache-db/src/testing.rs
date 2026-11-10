//! Fixtures for tests: an in-memory database with the whole schema.
//!
//! Other crates reach this through the `testing` feature, from their
//! dev-dependencies only.

use crate::migration::Migrator;
use sea_orm::{Database, DatabaseConnection};
use sea_orm_migration::MigratorTrait;

/// A fresh in-memory SQLite database, migrated to the current schema.
pub async fn memory_db() -> DatabaseConnection {
    let db = Database::connect("sqlite::memory:")
        .await
        .expect("an in-memory database");
    Migrator::up(&db, None).await.expect("the schema");
    db
}
