use crate::package::delete::package_delete;
use crate::repository::Repository;
use crate::snapshot::SnapshotStore;
use aurcache_db::dependencies;
use aurcache_db::packages;
use aurcache_db::prelude::{Dependencies, Packages};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, QuerySelect, Set,
};
use std::collections::{HashMap, HashSet};

/// Live-check what `from` was holding up: delete everything those packages
/// reach that no directly requested package still needs.
///
/// Reachability from the requested packages, rather than a per-package "does
/// anything still depend on me": that question cannot collect a dependency
/// cycle, because every package in one has a dependent — the next package
/// around the cycle. Two packages that only need each other would answer "keep
/// me" forever and stay in the database with nothing above them, which is what
/// this walks past.
///
/// The candidate set is still only what `from` reaches, so a change collects
/// what it orphaned and nothing else. Sweeping the whole graph would also pick
/// up rows an unrelated concurrent add has inserted but not yet linked up.
///
/// `hold` counts as needed beside the requested packages: a package in the
/// middle of an update is never collected from under it, even when a cycle
/// leads back to it.
///
/// Everything collected goes in one [`package_delete`], which is what lets a
/// chain or a cycle go at once: each member is still depended on, but only by
/// others going with it.
pub async fn live_check(
    db: &DatabaseConnection,
    store: &SnapshotStore,
    repo: &Repository,
    from: &[i32],
    hold: &[i32],
) -> anyhow::Result<()> {
    if from.is_empty() {
        return Ok(());
    }
    let dependees = dependency_edges(db).await?;

    // What this change could possibly have orphaned.
    let candidates = reachable_from(&dependees, from.iter().copied());

    // What the packages users actually asked for still need.
    let roots = Packages::find()
        .filter(packages::Column::DirectlyRequested.eq(true))
        .select_only()
        .column(packages::Column::Id)
        .into_tuple::<i32>()
        .all(db)
        .await?;
    let needed = reachable_from(&dependees, roots.into_iter().chain(hold.iter().copied()));

    let orphaned: Vec<i32> = candidates.difference(&needed).copied().collect();
    package_delete(db, store, repo, &orphaned).await
}

/// Every dependency link, as dependent -> the packages it needs.
async fn dependency_edges(db: &DatabaseConnection) -> anyhow::Result<HashMap<i32, Vec<i32>>> {
    let mut edges: HashMap<i32, Vec<i32>> = HashMap::new();
    for link in Dependencies::find()
        .select_only()
        .column(dependencies::Column::DependentId)
        .column(dependencies::Column::DependeeId)
        .into_tuple::<(i32, i32)>()
        .all(db)
        .await?
    {
        edges.entry(link.0).or_default().push(link.1);
    }
    Ok(edges)
}

/// The roots plus everything they need, transitively. Cycle-safe: an id is
/// expanded once.
fn reachable_from(
    dependees: &HashMap<i32, Vec<i32>>,
    roots: impl IntoIterator<Item = i32>,
) -> HashSet<i32> {
    let mut seen = HashSet::new();
    let mut queue: Vec<i32> = roots.into_iter().collect();
    while let Some(pkg_id) = queue.pop() {
        if !seen.insert(pkg_id) {
            continue;
        }
        if let Some(deps) = dependees.get(&pkg_id) {
            queue.extend(deps.iter().copied());
        }
    }
    seen
}

/// "Remove" a package: clear its directly_requested flag, then live-check it.
///
/// So a package something still depends on keeps everything -- rows,
/// artifacts, checkout -- and only stops being requested.
pub async fn package_remove(
    db: &DatabaseConnection,
    store: &SnapshotStore,
    repo: &Repository,
    pkg_id: i32,
) -> anyhow::Result<()> {
    let pkg = Packages::find_by_id(pkg_id)
        .one(db)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Package id {pkg_id} not found"))?;

    let mut active: packages::ActiveModel = pkg.into();
    active.directly_requested = Set(false);
    active.save(db).await?;

    live_check(db, store, repo, &[pkg_id], &[]).await
}
