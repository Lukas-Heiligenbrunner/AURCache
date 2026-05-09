use crate::package::delete::package_delete;
use aurcache_db::dependencies;
use aurcache_db::prelude::{Dependencies, Packages};
use aurcache_db::packages;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, PaginatorTrait, QueryFilter, Set,
};

/// Live-check a package: if it's not directly requested and nothing depends
/// on it, remove it and live-check its dependencies.
pub async fn live_check(db: &DatabaseConnection, pkg_id: i32) -> anyhow::Result<()> {
    let pkg = Packages::find_by_id(pkg_id)
        .one(db)
        .await?
        .ok_or(anyhow::anyhow!("Package id {pkg_id} not found"))?;

    // If directly requested by user, keep it
    if pkg.directly_requested != 0 {
        return Ok(());
    }

    // If any other package depends on this one, keep it
    let dependents = Dependencies::find()
        .filter(dependencies::Column::DependeeId.eq(pkg_id))
        .count(db)
        .await?;
    if dependents > 0 {
        return Ok(());
    }

    // Collect this package's dependencies before deleting it
    let my_deps: Vec<i32> = Dependencies::find()
        .filter(dependencies::Column::DependentId.eq(pkg_id))
        .all(db)
        .await?
        .into_iter()
        .map(|d| d.dependee_id)
        .collect();

    // Remove the package
    package_delete(db, pkg_id).await?;

    // Live-check each dependency
    for dep_id in my_deps {
        Box::pin(live_check(db, dep_id)).await?;
    }

    Ok(())
}

/// "Remove" a package: clear its directly_requested flag, then live-check it.
pub async fn package_remove(db: &DatabaseConnection, pkg_id: i32) -> anyhow::Result<()> {
    let pkg = Packages::find_by_id(pkg_id)
        .one(db)
        .await?
        .ok_or(anyhow::anyhow!("Package id {pkg_id} not found"))?;

    let mut active: packages::ActiveModel = pkg.into();
    active.directly_requested = Set(0);
    active.save(db).await?;

    live_check(db, pkg_id).await
}
