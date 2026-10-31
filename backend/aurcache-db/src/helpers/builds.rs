//! Build-row queries shared across the update, dependency and completion paths.

use crate::lists::Platforms;
use crate::prelude::Builds;
use crate::{builds, packages};
use aurcache_common::build_state::BuildState;
use pacman_mirrors::platforms::Platform;
use sea_orm::sea_query::{Alias, Expr, ExprTrait, Func, Query};
use sea_orm::{
    ColumnTrait, ConnectionTrait, DbErr, EntityTrait, Order, QueryFilter, QueryOrder, QuerySelect,
    RelationTrait, Select,
};
use std::collections::BTreeMap;

use aurcache_common::api::log::BuildRef;

/// How recent a build is: when it ended, or when it started where it has no
/// end. Every "newest successful build" query orders by this, descending, with
/// the id as the final tie-break.
///
/// One expression rather than the two columns in turn, because a `NULL` end
/// time sorts first under `DESC` on Postgres and last on SQLite: ordering by
/// `end_time` then `start_time` picked different builds on the two backends.
fn recency() -> Expr {
    Func::coalesce([
        Expr::col((builds::Entity, builds::Column::EndTime)),
        Expr::col((builds::Entity, builds::Column::StartTime)),
    ])
    .into()
}

/// The most recent *successful* build row selection, by [`recency`],
/// projecting one column.
///
/// The column is a parameter rather than a second `select_only` at the call
/// site: stacking `select_only` reads as resetting the projection, and the
/// next reader *will* "fix" it into returning two columns for a one-tuple.
fn newest_success_query(pkg_id: i32, column: builds::Column) -> Select<Builds> {
    Builds::find()
        .select_only()
        .column(column)
        .filter(builds::Column::PkgId.eq(pkg_id))
        .filter(builds::Column::Status.eq(BuildState::Successful))
        .order_by(recency(), Order::Desc)
        .order_by(builds::Column::Id, Order::Desc)
        .limit(1)
}

/// A version as read from `builds.version`, which is NOT NULL DEFAULT '': a
/// build that has not determined its version yet holds an empty string, which
/// means "not known", not "the empty version".
fn known_version(version: String) -> Option<String> {
    (!version.is_empty()).then_some(version)
}

/// The version of the most recently *successful* build of `pkg_id` on
/// `platform`.
///
/// `None` when no such build exists. Used to decide whether a dependency is
/// already satisfied by an existing build, which both the completion path
/// (`aurcache_utils::worker_complete`) and the update path
/// (`aurcache_utils::package::update`) ask — kept in one place so their
/// ordering cannot diverge.
pub async fn latest_successful_version<C: ConnectionTrait>(
    db: &C,
    pkg_id: i32,
    platform: Platform,
) -> Result<Option<String>, DbErr> {
    newest_success_query(pkg_id, builds::Column::Version)
        .filter(builds::Column::Platform.eq(platform))
        .into_tuple::<(String,)>()
        .one(db)
        .await
        .map(|row| row.and_then(|(version,)| known_version(version)))
}

/// Whether `dependee_id`'s newest successful build on `platform` satisfies
/// `constraint`.
///
/// The one answer to "is this dependency ready?". Two paths ask it — a build
/// finishing and deciding whether to promote its dependents, and a build being
/// queued and deciding whether it may start — and they used to run separate
/// queries that ordered differently: one by `end_time` alone, the other by
/// `end_time` with `start_time` as the tie-break. A build with no recorded end
/// time could therefore be picked by one and not the other, so the same
/// dependency read as ready to the queue and not ready to the builder.
pub async fn dependency_satisfied<C: ConnectionTrait>(
    db: &C,
    dependee_id: i32,
    platform: Platform,
    constraint: &str,
) -> Result<bool, DbErr> {
    Ok(latest_successful_version(db, dependee_id, platform)
        .await?
        .is_some_and(|version| aurcache_deps::satisfies_constraint(&version, constraint)))
}

/// Whether every one of `deps` is satisfied on `platform`; see
/// [`dependency_satisfied`].
pub async fn dependencies_satisfied<C: ConnectionTrait>(
    db: &C,
    deps: &[crate::dependencies::Model],
    platform: Platform,
) -> Result<bool, DbErr> {
    for dep in deps {
        if !dependency_satisfied(db, dep.dependee_id, platform, &dep.version_constraint).await? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// The build of `pkg_id` on `platform` that has not settled yet, if any.
///
/// At most one exists: a partial unique index on `builds(pkg_id, platform)`
/// covers these states.
pub async fn pending_build<C: ConnectionTrait>(
    db: &C,
    pkg_id: i32,
    platform: Platform,
) -> Result<Option<builds::Model>, DbErr> {
    Builds::find()
        .filter(builds::Column::PkgId.eq(pkg_id))
        .filter(builds::Column::Platform.eq(platform))
        .filter(builds::Column::Status.is_in(BuildState::IN_PROGRESS))
        .one(db)
        .await
}

/// The version of the most recently *successful* build of `pkg_id` across all
/// platforms.
///
/// Same shape as [`latest_successful_version`], but the package page is not
/// scoped to one platform, so the newest success over any of them stands for
/// the package.
pub async fn latest_successful_version_any_platform<C: ConnectionTrait>(
    db: &C,
    pkg_id: i32,
) -> Result<Option<String>, DbErr> {
    newest_success_query(pkg_id, builds::Column::Version)
        .into_tuple::<(String,)>()
        .one(db)
        .await
        .map(|row| row.and_then(|(version,)| known_version(version)))
}

/// What the most recently *successful* build of `pkg_id` was made from:
/// `source_url -> commit`, empty when nothing is recorded.
///
/// The baseline for "has upstream moved since we built it?". It has to be a
/// successful build: one that failed at a commit says nothing about what is in
/// the repository, and treating it as the baseline would leave the package
/// quietly sitting at a commit nothing ever produced -- the same trap
/// [`latest_successful_version`] exists to avoid for versions.
///
/// Empty means *unknown*, never "unchanged": a package built before this was
/// recorded, or whose sources could not be resolved, falls back to the version
/// check's own watermark rather than being declared up to date. Unparseable
/// JSON is treated the same way, since a stored string nobody can read is not
/// evidence either.
pub async fn latest_successful_build_vcs_sources<C: ConnectionTrait>(
    db: &C,
    pkg_id: i32,
) -> Result<BTreeMap<String, String>, DbErr> {
    latest_successful_build_vcs_sources_on(db, pkg_id, None).await
}

/// The successful source baseline on one architecture, or across all for summaries.
pub async fn latest_successful_build_vcs_sources_on<C: ConnectionTrait>(
    db: &C,
    pkg_id: i32,
    platform: Option<Platform>,
) -> Result<BTreeMap<String, String>, DbErr> {
    let query = newest_success_query(pkg_id, builds::Column::VcsSources);
    let query = if let Some(platform) = platform {
        query.filter(builds::Column::Platform.eq(platform))
    } else {
        query
    };
    let recorded = query
        .into_tuple::<Option<String>>()
        .one(db)
        .await?
        .flatten();
    let Some(json) = recorded else {
        return Ok(BTreeMap::new());
    };
    Ok(serde_json::from_str(&json).unwrap_or_else(|e| {
        tracing::warn!("build of package {pkg_id} has unreadable vcs_sources: {e}");
        BTreeMap::new()
    }))
}

/// Record what a build's VCS sources were at, replacing whatever was there --
/// the worker's report of what it checked out overwrites the guess made when
/// the build was queued.
///
/// An empty set clears the column rather than storing `{}`: "no sources" and
/// "sources unknown" both mean there is nothing to compare against, and one
/// spelling for that is enough.
pub async fn record_build_vcs_sources<C: ConnectionTrait>(
    db: &C,
    build_id: i32,
    commits: &BTreeMap<String, String>,
) -> Result<(), DbErr> {
    let json = if commits.is_empty() {
        None
    } else {
        Some(serde_json::to_string(commits).map_err(|e| DbErr::Custom(e.to_string()))?)
    };
    Builds::update_many()
        .col_expr(builds::Column::VcsSources, json.into())
        .filter(builds::Column::Id.eq(build_id))
        .exec(db)
        .await?;
    Ok(())
}

/// The repository version of the package row in the *enclosing* query, as a
/// correlated scalar subquery.
///
/// The same "successful builds only" rule as [`latest_successful_version_any_platform`],
/// expressed as a column so that a package listing reports it for every row
/// without one query per package.
///
/// Ordered by [`recency`] like every other "newest successful build" query,
/// and `NULLIF` for the reason [`known_version`] gives.
#[must_use]
pub fn latest_successful_version_expr() -> Expr {
    Expr::from(
        Query::select()
            .expr(Func::cust(Alias::new("NULLIF")).args([
                (Expr::col((builds::Entity, builds::Column::Version))),
                Expr::val(""),
            ]))
            .from(builds::Entity)
            .and_where(
                Expr::col((builds::Entity, builds::Column::PkgId))
                    .equals((packages::Entity, packages::Column::Id)),
            )
            .and_where(
                Expr::col((builds::Entity, builds::Column::Status)).eq(BuildState::Successful),
            )
            .order_by_expr(recency(), Order::Desc)
            .order_by((builds::Entity, builds::Column::Id), Order::Desc)
            .limit(1)
            .to_owned(),
    )
}

/// The status a package shows, from the states of its newest build on each
/// platform it is built for.
///
/// Anything still under way leads, the most active first -- a package building
/// on one platform and queued on another is building -- then a failure, then a
/// success. `None` when it has no build on any of them.
#[must_use]
pub fn combined_status(states: impl IntoIterator<Item = BuildState>) -> Option<BuildState> {
    const PRECEDENCE: [BuildState; 6] = [
        BuildState::Active,
        BuildState::Publishing,
        BuildState::Enqueued,
        BuildState::WaitingForDeps,
        BuildState::Failed,
        BuildState::Successful,
    ];
    let rank = |state: &BuildState| PRECEDENCE.iter().position(|s| s == state);
    states.into_iter().min_by_key(rank)
}

/// Bring `pkg_id`'s status in line with its builds; see [`combined_status`].
///
/// The one way `packages.status` is written once a package has builds, so it
/// cannot disagree with them: every change to a build's status calls this, in
/// the same transaction. A package with no build on any configured platform is
/// left as it is.
pub async fn refresh_package_status<C: ConnectionTrait>(db: &C, pkg_id: i32) -> Result<(), DbErr> {
    // Optional: a row written before the column had a default holds NULL.
    let Some(configured) = packages::Entity::find_by_id(pkg_id)
        .select_only()
        .column(packages::Column::Platforms)
        .lock_exclusive()
        .into_tuple::<Option<Platforms>>()
        .one(db)
        .await?
    else {
        return Ok(());
    };
    let configured = configured.unwrap_or_default();

    // The newest build per platform: the one whose number is the highest of
    // that package and platform.
    let latest = Alias::new("latest");
    let newest_number = Query::select()
        .expr(Expr::col((latest.clone(), builds::Column::Number)).max())
        .from_as(builds::Entity, latest.clone())
        .and_where(
            Expr::col((latest.clone(), builds::Column::PkgId))
                .equals((builds::Entity, builds::Column::PkgId)),
        )
        .and_where(
            Expr::col((latest, builds::Column::Platform))
                .equals((builds::Entity, builds::Column::Platform)),
        )
        .to_owned();
    let newest: Vec<(Platform, BuildState)> = Builds::find()
        .select_only()
        .column(builds::Column::Platform)
        .column(builds::Column::Status)
        .filter(builds::Column::PkgId.eq(pkg_id))
        .filter(Expr::col((builds::Entity, builds::Column::Number)).eq(Expr::from(newest_number)))
        .into_tuple()
        .all(db)
        .await?;

    let Some(status) = combined_status(
        newest
            .into_iter()
            .filter(|(platform, _)| configured.contains(*platform))
            .map(|(_, status)| status),
    ) else {
        return Ok(());
    };
    packages::Entity::update_many()
        .col_expr(packages::Column::Status, status.into())
        .filter(packages::Column::Id.eq(pkg_id))
        .exec(db)
        .await?;
    Ok(())
}

/// Delete a build row; its package's status follows, in the same transaction.
pub async fn delete_build<C: ConnectionTrait + sea_orm::TransactionTrait>(
    db: &C,
    build: &builds::Model,
) -> Result<(), DbErr> {
    let txn = db.begin().await?;
    Builds::delete_by_id(build.id).exec(&txn).await?;
    refresh_package_status(&txn, build.pkg_id).await?;
    sea_orm::TransactionSession::commit(txn).await
}

/// Name builds the way everything outside the database does: by package and
/// number, for row ids that mean nothing to anyone else.
///
/// A build whose package is gone cannot be named, and is left out.
pub async fn build_refs<C: ConnectionTrait>(db: &C, ids: &[i32]) -> Result<Vec<BuildRef>, DbErr> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    Ok(Builds::find()
        .select_only()
        .column(packages::Column::Name)
        .column(builds::Column::Number)
        .join(
            sea_orm::JoinType::InnerJoin,
            builds::Relation::Packages.def(),
        )
        .filter(builds::Column::Id.is_in(ids.iter().copied()))
        .order_by_asc(builds::Column::Id)
        .into_tuple::<(String, i32)>()
        .all(db)
        .await?
        .into_iter()
        .map(|(pkgbase, number)| BuildRef { pkgbase, number })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::{
        latest_successful_build_vcs_sources, latest_successful_version_any_platform,
        record_build_vcs_sources,
    };
    use crate::migration::Migrator;
    use aurcache_common::build_state::BuildState;
    use sea_orm::{
        ColumnTrait, ConnectionTrait, Database, DatabaseConnection, EntityTrait, QueryFilter,
        QuerySelect,
    };
    use sea_orm_migration::MigratorTrait;
    use std::collections::BTreeMap;

    async fn setup() -> DatabaseConnection {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();
        db.execute_unprepared("INSERT INTO packages (id, name) VALUES (1, 'cargo-diet')")
            .await
            .unwrap();
        db
    }

    /// One tracked source, spelled as `.SRCINFO` would.
    fn url() -> String {
        "git+https://example.test/repo.git".to_string()
    }

    /// A recorded set, in the shape the column holds.
    fn sources(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    /// The row id behind a build number, which is what the rows hang off.
    async fn build_id(db: &DatabaseConnection, number: i32) -> i32 {
        crate::prelude::Builds::find()
            .select_only()
            .column(crate::builds::Column::Id)
            .filter(crate::builds::Column::Number.eq(number))
            .into_tuple::<i32>()
            .one(db)
            .await
            .unwrap()
            .unwrap()
    }

    async fn build(
        db: &DatabaseConnection,
        number: i32,
        status: BuildState,
        version: &str,
        start: i64,
    ) {
        db.execute_unprepared(&format!(
            "INSERT INTO builds (pkg_id, number, status, start_time, platform, version) \
             VALUES (1, {number}, {}, {start}, 'x86_64', '{version}')",
            status.as_i32()
        ))
        .await
        .unwrap();
    }

    /// A failed build says nothing about what is in the repository.
    ///
    /// This is what made an out-of-date package unbuildable: upstream moved to
    /// 1.4.1-1, the build of it failed, and the update path -- which asked for
    /// the latest build of *any* outcome -- refused every retry with "already
    /// up to date (version 1.4.1-1)" about a version nothing had ever built.
    #[tokio::test]
    async fn a_failed_build_is_not_a_built_version() {
        let db = setup().await;
        build(&db, 6, BuildState::Failed, "1.4.1-1", 100).await;

        assert_eq!(
            latest_successful_version_any_platform(&db, 1)
                .await
                .unwrap(),
            None
        );
    }

    /// And a failed *newer* attempt does not hide the older success: the
    /// repository still serves what that success produced.
    #[tokio::test]
    async fn a_later_failure_does_not_hide_an_earlier_success() {
        let db = setup().await;
        build(&db, 5, BuildState::Successful, "1.4.0-1", 100).await;
        build(&db, 6, BuildState::Failed, "1.4.1-1", 200).await;

        assert_eq!(
            latest_successful_version_any_platform(&db, 1)
                .await
                .unwrap()
                .as_deref(),
            Some("1.4.0-1")
        );
    }

    /// A success at the version being asked for is what should stop a
    /// redundant rebuild, and still does.
    #[tokio::test]
    async fn a_successful_build_reports_its_version() {
        let db = setup().await;
        build(&db, 6, BuildState::Successful, "1.4.1-1", 100).await;

        assert_eq!(
            latest_successful_version_any_platform(&db, 1)
                .await
                .unwrap()
                .as_deref(),
            Some("1.4.1-1")
        );
    }

    /// The *successful* build is the baseline: what a failed attempt was made
    /// from is not what is in the repository.
    #[tokio::test]
    async fn the_baseline_is_what_the_last_successful_build_was_made_from() {
        let db = setup().await;
        build(&db, 5, BuildState::Successful, "r1.aaa-1", 100).await;
        build(&db, 6, BuildState::Failed, "r2.bbb-1", 200).await;
        let (success, failure) = (build_id(&db, 5).await, build_id(&db, 6).await);

        record_build_vcs_sources(&db, success, &sources(&[(&url(), "aaa")]))
            .await
            .unwrap();
        record_build_vcs_sources(&db, failure, &sources(&[(&url(), "bbb")]))
            .await
            .unwrap();

        assert_eq!(
            latest_successful_build_vcs_sources(&db, 1).await.unwrap(),
            sources(&[(&url(), "aaa")]),
            "the failed attempt's commit must not become the baseline"
        );
    }

    /// Nothing recorded is *unknown*, not "unchanged" — every build predates
    /// this column, and reading NULL as up-to-date would freeze them all.
    #[tokio::test]
    async fn a_build_with_no_record_reports_nothing() {
        let db = setup().await;
        build(&db, 5, BuildState::Successful, "r1.aaa-1", 100).await;

        assert!(
            latest_successful_build_vcs_sources(&db, 1)
                .await
                .unwrap()
                .is_empty()
        );
    }

    /// A PKGBUILD can carry several `git+` sources, and re-recording replaces
    /// the set — the worker's report of what it checked out overwrites the
    /// guess made when the build was queued.
    #[tokio::test]
    async fn several_sources_are_recorded_and_re_recording_replaces() {
        let db = setup().await;
        build(&db, 5, BuildState::Successful, "r1.aaa-1", 100).await;
        let id = build_id(&db, 5).await;

        let two = sources(&[
            ("git+https://example.test/a.git", "a1"),
            ("git+https://example.test/b.git", "b1"),
        ]);
        record_build_vcs_sources(&db, id, &two).await.unwrap();
        assert_eq!(
            latest_successful_build_vcs_sources(&db, 1).await.unwrap(),
            two
        );

        let one = sources(&[("git+https://example.test/a.git", "a2")]);
        record_build_vcs_sources(&db, id, &one).await.unwrap();
        assert_eq!(
            latest_successful_build_vcs_sources(&db, 1).await.unwrap(),
            one,
            "re-recording replaces the set rather than merging into it"
        );
    }

    /// An empty set clears the column instead of storing `{}`: "no sources" and
    /// "sources unknown" are the same answer to the only question asked of it.
    #[tokio::test]
    async fn recording_nothing_clears_the_record() {
        let db = setup().await;
        build(&db, 5, BuildState::Successful, "r1.aaa-1", 100).await;
        let id = build_id(&db, 5).await;
        record_build_vcs_sources(&db, id, &sources(&[(&url(), "aaa")]))
            .await
            .unwrap();

        record_build_vcs_sources(&db, id, &BTreeMap::new())
            .await
            .unwrap();

        assert!(
            latest_successful_build_vcs_sources(&db, 1)
                .await
                .unwrap()
                .is_empty()
        );
    }

    /// A stored string nobody can parse is not evidence of anything, so it
    /// reads as unknown rather than taking the version check down with it.
    #[tokio::test]
    async fn unreadable_json_reads_as_unknown() {
        let db = setup().await;
        build(&db, 5, BuildState::Successful, "r1.aaa-1", 100).await;
        let id = build_id(&db, 5).await;
        db.execute_unprepared(&format!(
            "UPDATE builds SET vcs_sources = 'not json' WHERE id = {id}"
        ))
        .await
        .unwrap();

        assert!(
            latest_successful_build_vcs_sources(&db, 1)
                .await
                .unwrap()
                .is_empty()
        );
    }

    /// A package's status follows its newest build on each configured
    /// platform: an older build does not count, and neither does a platform
    /// the package is no longer built for.
    #[tokio::test]
    async fn the_package_status_is_its_newest_builds() {
        use super::refresh_package_status;
        let db = setup().await;
        db.execute_unprepared("UPDATE packages SET platforms = 'x86_64' WHERE id = 1")
            .await
            .unwrap();
        let status = || async {
            crate::prelude::Packages::find_by_id(1)
                .select_only()
                .column(crate::packages::Column::Status)
                .into_tuple::<BuildState>()
                .one(&db)
                .await
                .unwrap()
                .unwrap()
        };

        build(&db, 1, BuildState::Failed, "1-1", 100).await;
        build(&db, 2, BuildState::Successful, "1-1", 200).await;
        refresh_package_status(&db, 1).await.unwrap();
        assert_eq!(
            status().await,
            BuildState::Successful,
            "the older failure is over"
        );

        db.execute_unprepared(&format!(
            "INSERT INTO builds (pkg_id, number, status, start_time, platform, version) \
             VALUES (1, 3, {}, 300, 'aarch64', '1-1')",
            BuildState::Failed.as_i32()
        ))
        .await
        .unwrap();
        refresh_package_status(&db, 1).await.unwrap();
        assert_eq!(
            status().await,
            BuildState::Successful,
            "aarch64 is not a platform it is built for"
        );

        build(&db, 4, BuildState::Enqueued, "1-2", 400).await;
        refresh_package_status(&db, 1).await.unwrap();
        assert_eq!(status().await, BuildState::Enqueued);
    }

    /// Both forms of "newest successful build" order the same way on both
    /// backends: by `COALESCE(end_time, start_time)` and the id, never by a
    /// bare column whose `NULL`s Postgres and SQLite sort to opposite ends.
    #[test]
    fn newest_success_orders_alike_on_every_backend() {
        use crate::builds;
        use sea_orm::sea_query::{PostgresQueryBuilder, Query, SqliteQueryBuilder};
        use sea_orm::{DatabaseBackend, QueryTrait};

        for backend in [DatabaseBackend::Sqlite, DatabaseBackend::Postgres] {
            let select = super::newest_success_query(1, builds::Column::Version)
                .build(backend)
                .to_string();
            let subquery = match backend {
                DatabaseBackend::Postgres => Query::select()
                    .expr(super::latest_successful_version_expr())
                    .to_string(PostgresQueryBuilder),
                _ => Query::select()
                    .expr(super::latest_successful_version_expr())
                    .to_string(SqliteQueryBuilder),
            };
            for sql in [select, subquery] {
                let order = &sql[sql.find("ORDER BY").expect("ordered")..];
                assert!(order.contains("COALESCE"), "{backend:?}: {sql}");
                assert!(order.contains("\"id\" DESC"), "{backend:?}: {sql}");
            }
        }
    }
}
