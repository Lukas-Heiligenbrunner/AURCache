//! Starting transactions that read before they write.

use sea_orm::{DbErr, SqliteTransactionMode, TransactionOptions, TransactionTrait};

/// Begin a transaction that reads and then writes, taking SQLite's write lock
/// up front.
///
/// SQLite's default `BEGIN` is deferred: the transaction reads from a snapshot
/// and asks for the write lock only at its first write. Under WAL, when another
/// connection has committed since that snapshot was taken, the upgrade fails at
/// once with `SQLITE_BUSY_SNAPSHOT`, and `busy_timeout` cannot help: waiting
/// would not make the snapshot current. `BEGIN IMMEDIATE` takes the lock before
/// the first read, waiting for it under `busy_timeout` like any other writer.
///
/// A transaction whose first statement is a write does not need this. The mode
/// is SQLite's alone; Postgres ignores it, and a nested call is a savepoint.
pub async fn begin_write<C: TransactionTrait>(db: &C) -> Result<C::Transaction, DbErr> {
    db.begin_with_options(TransactionOptions {
        sqlite_transaction_mode: Some(SqliteTransactionMode::Immediate),
        ..Default::default()
    })
    .await
}
