//! One list of the architectures a worker builds.
//!
//! Workers registered native and emulated architectures apart, and the router
//! kept an emulated one from a worker while another built it natively. How a
//! worker builds is its own business, though -- emulation is one way, a
//! dispatcher to machines of each architecture another -- so it now reports
//! what it builds and nothing about how. What a worker had registered as
//! emulated it still builds, so it joins the rest.

use sea_orm::{ConnectionTrait, FromQueryResult};
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[derive(FromQueryResult)]
struct Arches {
    id: i32,
    arches: String,
    emulated_arches: String,
}

/// Both columns are `,`-joined, as `WorkerList` writes them.
fn merged(row: &Arches) -> String {
    let mut out: Vec<&str> = Vec::new();
    for arch in row.arches.split(',').chain(row.emulated_arches.split(',')) {
        let arch = arch.trim();
        if !arch.is_empty() && !out.contains(&arch) {
            out.push(arch);
        }
    }
    out.join(",")
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let workers = Alias::new("workers");
        manager
            .alter_table(
                Table::alter()
                    .table(workers.clone())
                    .rename_column(Alias::new("native_arches"), Alias::new("arches"))
                    .to_owned(),
            )
            .await?;

        let db = manager.get_connection();
        let backend = manager.get_database_backend();
        let rows = Arches::find_by_statement(
            backend.build(
                Query::select()
                    .columns([
                        Alias::new("id"),
                        Alias::new("arches"),
                        Alias::new("emulated_arches"),
                    ])
                    .from(workers.clone())
                    .and_where(Expr::col(Alias::new("emulated_arches")).ne("")),
            ),
        )
        .all(db)
        .await?;
        for row in &rows {
            db.execute(
                Query::update()
                    .table(workers.clone())
                    .value(Alias::new("arches"), merged(row))
                    .and_where(Expr::col(Alias::new("id")).eq(row.id)),
            )
            .await?;
        }

        manager
            .alter_table(
                Table::alter()
                    .table(workers)
                    .drop_column(Alias::new("emulated_arches"))
                    .to_owned(),
            )
            .await
    }
}

#[cfg(test)]
mod tests {
    use crate::migration::Migrator;
    use sea_orm::{ConnectionTrait, Database, DbBackend, Statement};
    use sea_orm_migration::MigratorTrait;

    #[tokio::test]
    async fn emulated_architectures_join_the_others() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, Some(Migrator::migrations().len() as u32 - 1))
            .await
            .unwrap();
        db.execute_unprepared(
            "INSERT INTO workers (id, name, status, cert_fingerprint, native_arches, emulated_arches, \
             settings_declaration) \
             VALUES (1, 'a', 1, 'fa', 'x86_64', 'aarch64,x86_64', '[]'), \
                    (2, 'b', 1, 'fb', 'aarch64', '', '[]')",
        )
        .await
        .unwrap();

        Migrator::up(&db, None).await.unwrap();

        let rows = db
            .query_all_raw(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT arches FROM workers ORDER BY id",
            ))
            .await
            .unwrap();
        let arches: Vec<String> = rows
            .iter()
            .map(|r| r.try_get("", "arches").unwrap())
            .collect();
        assert_eq!(arches, ["x86_64,aarch64", "aarch64"]);
    }
}
