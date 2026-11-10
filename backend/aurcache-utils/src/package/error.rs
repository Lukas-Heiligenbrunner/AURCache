//! Why adding or updating a package failed, told apart by whose it is to fix.

use std::fmt;

/// Why a package could not be added or updated.
#[derive(Debug)]
pub enum PackageError {
    /// The package's source cannot be used as it stands: an unknown AUR name,
    /// a remote that cannot be fetched, a PKGBUILD that does not parse, a patch
    /// that no longer applies, a dependency that does not resolve.
    ///
    /// Also whatever else is not recognised as the server's: nearly everything
    /// these flows do is fetching, parsing and resolving the source, and a
    /// server-side failure of another kind (a full disk, say) lands here, as
    /// every failure did before the two were told apart.
    Source(anyhow::Error),
    /// The server's own failure: its database could not be read or written,
    /// or a task of its own died.
    Internal(anyhow::Error),
}

impl From<anyhow::Error> for PackageError {
    /// A failure of the database underneath, or a panicked blocking task, is
    /// the server's wherever in the flow it happened, as long as it stays in
    /// the error's chain: wrap with `.context()`, never by formatting the error
    /// into a new message.
    fn from(error: anyhow::Error) -> Self {
        let internal = |cause: &(dyn std::error::Error + 'static)| {
            cause.is::<sea_orm::DbErr>() || cause.is::<tokio::task::JoinError>()
        };
        if error.chain().any(internal) {
            Self::Internal(error)
        } else {
            Self::Source(error)
        }
    }
}

impl From<sea_orm::DbErr> for PackageError {
    fn from(error: sea_orm::DbErr) -> Self {
        Self::Internal(error.into())
    }
}

impl fmt::Display for PackageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Source(e) | Self::Internal(e) => write!(f, "{e:#}"),
        }
    }
}

impl std::error::Error for PackageError {}

#[cfg(test)]
mod tests {
    use super::PackageError;
    use anyhow::Context;

    #[test]
    fn a_database_failure_is_internal_under_context() {
        let failure: anyhow::Result<()> = Err(sea_orm::DbErr::Custom("gone".into()).into());
        let error = failure.context("recording the dependencies").unwrap_err();
        assert!(matches!(
            PackageError::from(error),
            PackageError::Internal(_)
        ));
    }

    #[tokio::test]
    async fn a_task_that_died_is_internal() {
        let died = tokio::task::spawn_blocking(|| panic!("in the checkout"))
            .await
            .unwrap_err();
        let error = anyhow::Error::from(died).context("reading the PKGBUILD");
        assert!(matches!(
            PackageError::from(error),
            PackageError::Internal(_)
        ));
    }

    #[test]
    fn anything_else_is_the_source() {
        let error = anyhow::anyhow!("patch does not apply");
        assert!(matches!(PackageError::from(error), PackageError::Source(_)));
    }
}
