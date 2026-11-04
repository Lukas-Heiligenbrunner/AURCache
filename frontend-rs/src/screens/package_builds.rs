//! Every build of one package.
//!
//! The package page shows only the latest build and the one currently in the
//! repository; this is where the rest of the history lives.

use crate::api::client;
use crate::list_controls::{ListHeader, Rows, Selection, use_rows};
use crate::listing::{Sort, SortDir, SortKey, sort_builds};
use crate::screens::builds::{BuildTable, BuildsOf};
use crate::widgets::Spinner;
use aurcache_client::Build;
use dioxus::prelude::*;

/// Newest first, as elsewhere: a build history is read as a log.
const DEFAULT_SORT: Sort = Sort {
    key: SortKey::Time,
    dir: SortDir::Desc,
};

/// No limit: the page is paged in the browser, and a capped fetch would end a
/// long history silently.
async fn load(pkgbase: String) -> Result<Vec<Build>, String> {
    client()?
        .list_builds(&aurcache_client::BuildQuery {
            pkgbase: Some(pkgbase),
            ..Default::default()
        })
        .await
        .map_err(|e| e.to_string())
}

#[component]
pub fn PackageBuilds(pkgbase: String) -> Element {
    // `use_reactive`, like the scope's own fetch, so the list follows the route.
    let builds = use_resource(use_reactive(&pkgbase, load));

    // Kept fresh like every other list: briskly while a build is under way,
    // so its row moves on without a reload, and at once when an add or a
    // rebuild queues one.
    let building = matches!(&*builds.read_unchecked(), Some(Ok(list))
        if list.iter().any(|b| b.status.is_in_progress()));
    crate::poll::use_poll(builds, building);
    crate::poll::use_refetch_on_package_change(builds);

    let mut page = use_signal(|| 0usize);
    let sort = use_signal(|| DEFAULT_SORT);
    // Sorted before paging, exactly as on the main builds screen, so the order
    // the header promises is the order the whole list is read in.
    let rows = use_rows(builds, page, move |list| {
        let mut shown = list.to_vec();
        sort_builds(&mut shown, sort());
        Selection {
            total: shown.len(),
            shown,
        }
    });

    // A different order is a different statement, not a new page.
    use_effect(use_reactive(&sort(), move |_| page.set(0)));

    rsx! {
        div { class: "card bg-base-100 shadow-xl",
            div { class: "card-body",
                ListHeader { title: "Builds" }

                match &*rows.read() {
                    Rows::Loading => rsx! { Spinner {} },
                    Rows::Failed(e) => rsx! {
                        div { class: "alert alert-error", span { "Could not load builds: {e}" } }
                    },
                    Rows::Ready { total: 0, .. } => rsx! {
                        div { class: "alert", span { "This package has never been built." } }
                    },
                    Rows::Ready { total, current, .. } => rsx! {
                        BuildTable {
                            builds: current.items.clone(),
                            info: current.info,
                            total: *total,
                            page,
                            sort,
                            of: BuildsOf::OnePackage,
                        }
                    },
                }
            }
        }
    }
}
