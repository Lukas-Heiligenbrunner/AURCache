//! Every build of one package.
//!
//! The package page shows only the latest build and the one currently in the
//! repository; this is where the rest of the history lives.

use crate::api::{LoadError, client};
use crate::dates::DateOnly;
use crate::format::format_duration;
use crate::listing::{
    LIST_TABLE, ListHeader, Pager, Sort, SortDir, SortKey, SortableHeader, paginate, sort_builds,
};
use crate::routes::Route;
use crate::screens::builds::{build_peak_memory, build_size};
use crate::status::BuildStatusBadge;
use aurcache_client::Build;
use dioxus::prelude::*;

const WIDE_ONLY: &str = "hidden md:table-cell";

/// Newest first, as elsewhere: a build history is read as a log.
const DEFAULT_SORT: Sort = Sort {
    key: SortKey::Time,
    dir: SortDir::Desc,
};

async fn load(pkgbase: String) -> Result<Vec<Build>, String> {
    client()?
        // A sub-resource rather than a query filter: a pkgbase may contain `+`,
        // which is literal in a path but decodes to a space in a query value.
        // No limit: the page is paged in the browser, and a capped fetch
        // would have silently ended a long history at 100.
        .list_builds(&aurcache_client::BuildQuery {
            pkgbase: Some(pkgbase.clone()),
            ..Default::default()
        })
        .await
        .map_err(|e| e.to_string())
}

#[component]
pub fn PackageBuilds(pkgbase: String) -> Element {
    // `use_reactive` so the fetch follows the route. Navigating between two
    // packages reuses this component -- same route, different parameter -- and
    // a resource whose closure captured the old name simply never re-runs: the
    // URL changes, no request is made, and the previous package stays on
    // screen looking like the one that was clicked.
    let mut package = use_resource(use_reactive(&pkgbase, |pkgbase| async move {
        Ok::<_, LoadError>(crate::api::client()?.get_package(&pkgbase).await?)
    }));

    // The same rule as the package page: a package that is not here is one
    // you might want to add.
    let notice = crate::notice::use_notice();
    use_effect(use_reactive(&pkgbase, move |pkgbase| {
        if matches!(&*package.read(), Some(Err(LoadError::NotFound))) {
            crate::notice::redirect(
                notice,
                Route::PackageAdd { q: pkgbase.clone() },
                crate::notice::Level::Info,
                format!("No package called {pkgbase} is tracked here. Search the AUR to add it."),
            );
        }
    }));

    let mut builds = use_resource(use_reactive(&pkgbase, load));
    let mut page = use_signal(|| 0usize);
    let sort = use_signal(|| DEFAULT_SORT);

    // A different order is a different statement, not a new page.
    use_effect(use_reactive(&sort(), move |_| page.set(0)));

    rsx! {
        div { class: "space-y-4",
        match &*package.read_unchecked() {
            Some(Ok(pkg)) => rsx! {
                crate::screens::PackageHeader {
                    pkg: pkg.clone(),
                    trail: vec![("Builds".to_string(), None)],
                    on_rebuilt: move |()| {
                        package.restart();
                        builds.restart();
                    },
                }
            },
            _ => rsx! {},
        }
        div { class: "card bg-base-100 shadow-xl",
            div { class: "card-body",
                ListHeader { title: "Builds" }

                match &*builds.read_unchecked() {
                    None => rsx! {
                        div { class: "flex justify-center p-8",
                            span { class: "loading loading-spinner loading-lg" }
                        }
                    },
                    Some(Err(e)) => rsx! {
                        div { class: "alert alert-error", span { "Could not load builds: {e}" } }
                    },
                    Some(Ok(list)) if list.is_empty() => rsx! {
                        div { class: "alert", span { "This package has never been built." } }
                    },
                    Some(Ok(list)) => {
                        let mut sorted = list.clone();
                        // What `after` a package's own builds would put at the
                        // top: sorting happens before paging, exactly as on the
                        // main builds screen, so the order the header promises
                        // is the order the whole list is read in.
                        sort_builds(&mut sorted, sort());
                        let current = paginate(&sorted, page());
                        rsx! {
                        div { class: "overflow-x-auto",
                            table { class: LIST_TABLE,
                                thead {
                                    tr {
                                        SortableHeader { label: "Build", column: SortKey::Name, sort, class: "" }
                                        th { class: "{WIDE_ONLY}", "Version" }
                                        SortableHeader { label: "Started", column: SortKey::Time, sort, class: "{WIDE_ONLY}" }
                                        SortableHeader { label: "Duration", column: SortKey::Duration, sort, class: "{WIDE_ONLY}" }
                                        th { class: "{WIDE_ONLY}", "Platform" }
                                        SortableHeader { label: "Worker", column: SortKey::Worker, sort, class: "{WIDE_ONLY}" }
                                        SortableHeader { label: "Size", column: SortKey::Size, sort, class: "{WIDE_ONLY} text-right" }
                                        SortableHeader { label: "Peak RAM", column: SortKey::Memory, sort, class: "{WIDE_ONLY} text-right" }
                                        SortableHeader { label: "Status", column: SortKey::Status, sort, class: "" }
                                    }
                                }
                                tbody {
                                    for build in current.items.iter() {
                                        tr {
                                            key: "{build.number}",
                                            class: "hover cursor-pointer",
                                            // The row is the build, so the
                                            // whole row navigates.
                                            onclick: {
                                                let pkgbase = build.pkg_name.clone();
                                                let number = build.number;
                                                move |_| {
                                                    navigator().push(Route::Build {
                                                        pkgbase: pkgbase.clone(),
                                                        number,
                                                    });
                                                }
                                            },
                                            td {
                                                Link {
                                                    // Not `link link-primary`:
                                                    // when every cell navigates,
                                                    // underlining one implies
                                                    // the rest do not. It stays
                                                    // a link so the address is
                                                    // copyable, middle-click
                                                    // opens a tab, and there is
                                                    // something to focus.
                                                    class: "font-mono",
                                                    to: Route::Build {
                                                        pkgbase: build.pkg_name.clone(),
                                                        number: build.number,
                                                    },
                                                    // Otherwise the click
                                                    // reaches the row too and
                                                    // pushes the same route
                                                    // twice, leaving a
                                                    // duplicate history entry.
                                                    onclick: move |e: MouseEvent| e.stop_propagation(),
                                                    "{build.number}"
                                                }
                                                // What the dropped columns said,
                                                // on a screen too narrow for them.
                                                div { class: "md:hidden text-xs opacity-60 [overflow-wrap:anywhere]",
                                                    span { class: "font-mono", "{build.version}" }
                                                    " · "
                                                    DateOnly { ts: build.start_time }
                                                    " · {format_duration(build.elapsed())}"
                                                }
                                            }
                                            td { class: "{WIDE_ONLY} font-mono text-sm", "{build.version}" }
                                            td { class: "{WIDE_ONLY} text-sm opacity-70",
                                                DateOnly { ts: build.start_time }
                                            }
                                            td { class: "{WIDE_ONLY} font-mono text-sm opacity-70",
                                                {format_duration(build.elapsed())}
                                            }
                                            td { class: "{WIDE_ONLY} text-sm opacity-70", "{build.platform}" }
                                            td { class: "{WIDE_ONLY} text-sm opacity-70",
                                                if let Some(worker) = build.worker_name.as_deref() {
                                                    "{worker}"
                                                } else {
                                                    // Not "no worker": nobody
                                                    // has claimed it yet.
                                                    span {
                                                        class: "opacity-40",
                                                        title: "Not claimed by a worker yet.",
                                                        "—"
                                                    }
                                                }
                                            }
                                            td { class: "{WIDE_ONLY} text-right font-mono text-sm opacity-70",
                                                {build_size(build)}
                                            }
                                            td {
                                                class: "{WIDE_ONLY} text-right font-mono text-sm opacity-70",
                                                title: "Peak memory of the build's whole process tree, from its cgroup.",
                                                {build_peak_memory(build)}
                                            }
                                            td { BuildStatusBadge { status: build.status } }
                                        }
                                    }
                                }
                            }
                        }
                        Pager {
                            page,
                            index: current.index,
                            pages: current.pages,
                            first: current.first,
                            count: current.items.len(),
                            total: current.total,
                        }
                        if current.pages <= 1 {
                            div { class: "text-sm opacity-60 pt-2", "{list.len()} builds" }
                        }
                    }
                    },
                }
            }
        }
        }
    }
}
