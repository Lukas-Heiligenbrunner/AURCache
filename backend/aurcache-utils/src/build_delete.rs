//! Removing a build from the history at an operator's request.

use crate::build_logger::remove_build_log;
use aurcache_common::build_state::BuildState;
use aurcache_db::builds;
use sea_orm::DatabaseConnection;

/// What [`delete_build`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delete {
    /// The row and its log are gone.
    Deleted,
    /// The build is running or being published, and was left alone.
    Running,
}

/// Delete `build`, which `pkgbase` names, and its log.
///
/// Refused while a worker holds the build's lease or the server is moving it
/// into the repository: removing the row under either leaves that work
/// pointing at nothing. Cancel it first.
pub async fn delete_build(
    db: &DatabaseConnection,
    pkgbase: &str,
    build: &builds::Model,
) -> anyhow::Result<Delete> {
    if matches!(build.status, BuildState::Active | BuildState::Publishing) {
        return Ok(Delete::Running);
    }
    aurcache_db::helpers::builds::delete_build(db, build).await?;
    remove_build_log(pkgbase, build.number).await;
    Ok(Delete::Deleted)
}
