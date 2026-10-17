//! Queries over a package's built artifacts.

use crate::{files, packages};
use sea_orm::sea_query::{Alias, Asterisk, CaseStatement, Expr, ExprTrait, Query};
use sea_orm::{ConnectionTrait, DbErr, EntityTrait, FromQueryResult, QuerySelect};

/// Total size of every artifact belonging to the package row in the *enclosing*
/// query, as a correlated scalar subquery.
///
/// All-or-nothing, matching what the package page applies to its own total: the
/// `CASE` yields NULL unless every artifact has a recorded size, so a partial
/// sum never reaches the column -- a sum of only the known parts reads as a
/// wrong number rather than as missing data. A package with no artifacts sums
/// over no rows and is NULL too, which is what "nothing built yet" should show.
///
/// Cast to `BIGINT` because Postgres widens `SUM(bigint)` to `numeric`, which
/// does not decode into an `i64`; SQLite reads the cast as its own INTEGER
/// affinity and is unaffected.
#[must_use]
pub fn total_artifact_size_expr() -> Expr {
    Expr::from(
        Query::select()
            .expr(
                CaseStatement::new().case(
                    Expr::col(Asterisk)
                        .count()
                        .eq(Expr::col((files::Entity, files::Column::Size)).count()),
                    Expr::col((files::Entity, files::Column::Size))
                        .sum()
                        .cast_as(Alias::new("BIGINT")),
                ),
            )
            .from(files::Entity)
            .and_where(
                Expr::col((files::Entity, files::Column::PackageId))
                    .equals((packages::Entity, packages::Column::Id)),
            )
            .to_owned(),
    )
}

/// Total size of every artifact in the repository.
///
/// All-or-nothing like [`total_artifact_size_expr`]: `None` while any artifact
/// has no recorded size. An empty repository is zero bytes, not unknown.
pub async fn total_size<C: ConnectionTrait>(db: &C) -> Result<Option<i64>, DbErr> {
    #[derive(FromQueryResult)]
    struct Totals {
        files: i64,
        sized: i64,
        bytes: Option<i64>,
    }
    let totals = files::Entity::find()
        .select_only()
        .column_as(Expr::col(Asterisk).count(), "files")
        .column_as(Expr::col(files::Column::Size).count(), "sized")
        .column_as(
            Expr::col(files::Column::Size)
                .sum()
                .cast_as(Alias::new("BIGINT")),
            "bytes",
        )
        .into_model::<Totals>()
        .one(db)
        .await?;
    Ok(match totals {
        None => Some(0),
        Some(totals) if totals.sized == totals.files => Some(totals.bytes.unwrap_or(0)),
        Some(_) => None,
    })
}

#[cfg(test)]
mod tests {
    use super::total_size;
    use crate::migration::Migrator;
    use crate::packages::SourceData;
    use crate::{files, packages};
    use aurcache_common::build_state::BuildState;
    use pacman_mirrors::platforms::Platform;
    use sea_orm::{ActiveModelTrait, Database, DatabaseConnection, Set};
    use sea_orm_migration::MigratorTrait;

    async fn repository(sizes: &[Option<i64>]) -> DatabaseConnection {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();
        let package = packages::ActiveModel {
            name: Set("hello".to_string()),
            status: Set(BuildState::Successful),
            out_of_date: Set(false),
            build_flags: Set(Default::default()),
            platforms: Set("x86_64".parse().unwrap()),
            source_data: Set(SourceData::Aur {
                name: "hello".into(),
            }),
            directly_requested: Set(true),
            ..Default::default()
        }
        .insert(&db)
        .await
        .unwrap();
        for (n, &size) in sizes.iter().enumerate() {
            files::ActiveModel {
                filename: Set(format!("hello-{n}.pkg.tar.zst")),
                platform: Set(Platform::X86_64),
                package_id: Set(package.id),
                size: Set(size),
                ..Default::default()
            }
            .insert(&db)
            .await
            .unwrap();
        }
        db
    }

    #[tokio::test]
    async fn an_empty_repository_is_zero_bytes() {
        assert_eq!(total_size(&repository(&[]).await).await.unwrap(), Some(0));
    }

    #[tokio::test]
    async fn the_total_is_every_artifact() {
        let db = repository(&[Some(3), Some(4)]).await;
        assert_eq!(total_size(&db).await.unwrap(), Some(7));
    }

    /// A partial sum would read as a wrong number, not as missing data.
    #[tokio::test]
    async fn one_unknown_size_makes_the_total_unknown() {
        let db = repository(&[Some(3), None]).await;
        assert_eq!(total_size(&db).await.unwrap(), None);
    }
}
