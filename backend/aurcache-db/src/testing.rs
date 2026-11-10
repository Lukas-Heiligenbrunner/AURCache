//! Fixtures for tests: an in-memory database with the whole schema, and
//! builders for the rows most tests start from.
//!
//! Other crates reach this through the `testing` feature, from their
//! dev-dependencies only.
//!
//! ```ignore
//! let db = memory_db().await;
//! let parent = package("parent").insert(&db).await;
//! let child = package("child").dependency_only().insert(&db).await;
//! depend(&db, &parent, &child, ">=1.0").await;
//! build(&child).status(BuildState::Failed).insert(&db).await;
//! ```

use crate::migration::Migrator;
use crate::{builds, dependencies, packages};
use aurcache_common::build_state::BuildState;
use aurcache_common::source::SourceData;
use pacman_mirrors::platforms::Platform;
use sea_orm::ActiveValue::Set;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, Database, DatabaseConnection, EntityTrait,
    QueryFilter, QueryOrder,
};
use sea_orm_migration::MigratorTrait;

/// A fresh in-memory SQLite database, migrated to the current schema.
pub async fn memory_db() -> DatabaseConnection {
    let db = Database::connect("sqlite::memory:")
        .await
        .expect("an in-memory database");
    Migrator::up(&db, None).await.expect("the schema");
    db
}

/// A package row named `name`, as most tests want one: an AUR package of that
/// name, for x86_64, requested directly, at `1.0.0`, its status `Successful`.
///
/// The status is written, not derived: insert a [`build`] too when the code
/// under test reads the builds behind it.
pub fn package(name: &str) -> NewPackage {
    NewPackage(packages::ActiveModel {
        name: Set(name.to_string()),
        status: Set(BuildState::Successful),
        out_of_date: Set(false),
        upstream_version: Set(Some("1.0.0".to_string())),
        build_flags: Set("--noconfirm;--noprogressbar".parse().expect("build flags")),
        platforms: Set("x86_64".parse().expect("platforms")),
        source_data: Set(SourceData::Aur { name: name.into() }),
        directly_requested: Set(true),
        ..Default::default()
    })
}

/// A package row to insert; see [`package`].
#[must_use]
pub struct NewPackage(packages::ActiveModel);

impl NewPackage {
    pub fn status(mut self, status: BuildState) -> Self {
        self.0.status = Set(status);
        self
    }

    /// In the database only because another package depends on it.
    pub fn dependency_only(mut self) -> Self {
        self.0.directly_requested = Set(false);
        self
    }

    /// Built for `platforms`, written as the column is: `x86_64;aarch64`.
    pub fn platforms(mut self, platforms: &str) -> Self {
        self.0.platforms = Set(platforms.parse().expect("platforms"));
        self
    }

    /// Upstream is at `version`.
    pub fn version(mut self, version: &str) -> Self {
        self.0.upstream_version = Set(Some(version.to_string()));
        self
    }

    pub fn out_of_date(mut self) -> Self {
        self.0.out_of_date = Set(true);
        self
    }

    pub fn source(mut self, source: SourceData) -> Self {
        self.0.source_data = Set(source);
        self
    }

    /// Any other column.
    pub fn with(mut self, change: impl FnOnce(&mut packages::ActiveModel)) -> Self {
        change(&mut self.0);
        self
    }

    pub async fn insert(self, db: &impl ConnectionTrait) -> packages::Model {
        self.0.insert(db).await.expect("the package row")
    }
}

/// A build of `package`, as most tests want one: successful, for x86_64, of
/// `1.0.0`, started and finished.
pub fn build(package: &packages::Model) -> NewBuild {
    NewBuild(builds::ActiveModel {
        pkg_id: Set(package.id),
        status: Set(BuildState::Successful),
        platform: Set(Platform::X86_64),
        version: Set("1.0.0".to_string()),
        start_time: Set(Some(1)),
        end_time: Set(Some(2)),
        ..Default::default()
    })
}

/// A build row to insert; see [`build`].
#[must_use]
pub struct NewBuild(builds::ActiveModel);

impl NewBuild {
    pub fn status(mut self, status: BuildState) -> Self {
        self.0.status = Set(status);
        self
    }

    pub fn platform(mut self, platform: Platform) -> Self {
        self.0.platform = Set(platform);
        self
    }

    pub fn version(mut self, version: &str) -> Self {
        self.0.version = Set(version.to_string());
        self
    }

    /// Any other column.
    pub fn with(mut self, change: impl FnOnce(&mut builds::ActiveModel)) -> Self {
        change(&mut self.0);
        self
    }

    /// Numbered after the package's last build unless a number was given, as
    /// the server numbers them.
    pub async fn insert(mut self, db: &impl ConnectionTrait) -> builds::Model {
        if self.0.number.is_not_set() {
            let last = builds::Entity::find()
                .filter(builds::Column::PkgId.eq(self.0.pkg_id.clone().unwrap()))
                .order_by_desc(builds::Column::Number)
                .one(db)
                .await
                .expect("the package's builds");
            self.0.number = Set(last.map_or(1, |build| build.number + 1));
        }
        self.0.insert(db).await.expect("the build row")
    }
}

/// Record that `dependent` depends on `dependee`, as `constraint` says
/// (`>=1.0`, or empty for any version).
pub async fn depend(
    db: &impl ConnectionTrait,
    dependent: &packages::Model,
    dependee: &packages::Model,
    constraint: &str,
) -> dependencies::Model {
    dependencies::ActiveModel {
        dependent_id: Set(dependent.id),
        dependee_id: Set(dependee.id),
        version_constraint: Set(constraint.to_string()),
        ..Default::default()
    }
    .insert(db)
    .await
    .expect("the dependency row")
}

#[cfg(test)]
mod tests {
    use super::{build, depend, memory_db, package};
    use aurcache_common::build_state::BuildState;

    #[tokio::test]
    async fn the_fixtures_make_rows_the_schema_accepts() {
        let db = memory_db().await;
        let parent = package("parent").insert(&db).await;
        let child = package("child")
            .dependency_only()
            .platforms("x86_64;aarch64")
            .insert(&db)
            .await;
        assert!(!child.directly_requested);
        assert_eq!(child.platforms.as_slice().len(), 2);
        let edge = depend(&db, &parent, &child, ">=1.0").await;
        assert_eq!(edge.dependee_id, child.id);
        let built = build(&child).status(BuildState::Failed).insert(&db).await;
        assert_eq!(built.pkg_id, child.id);
        assert_eq!(built.status, BuildState::Failed);
        let again = build(&child).insert(&db).await;
        assert_eq!((built.number, again.number), (1, 2));
    }
}
