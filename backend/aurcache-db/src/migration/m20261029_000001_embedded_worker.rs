//! Which worker row is the hybrid image's embedded worker.
//!
//! The embedded worker's identity lives in an anonymous volume, which an
//! orchestrator that recreates containers -- TrueNAS among them -- discards
//! every time. It came back as a new worker on each start, leaving the old row
//! behind. Marked, its row can be re-keyed instead: see
//! `aurcache_api::worker::register_worker`.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(Alias::new("workers"))
                    .add_column(
                        ColumnDef::new(Alias::new("embedded"))
                            .boolean()
                            .not_null()
                            .default(false),
                    )
                    .to_owned(),
            )
            .await
    }
}
