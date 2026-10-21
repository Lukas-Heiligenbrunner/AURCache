//! The builds list.

use crate::api::client;
use crate::dates::DateOnly;
use crate::format::format_duration;
use crate::listing::{
    LIST_TABLE, ListControls, ListHeader, Page, Pager, Sort, SortDir, SortKey, SortableHeader,
    ViewParams, filter_builds, paginate, sort_builds, use_url_search, use_url_view,
};
use crate::routes::Route;
use crate::status::BuildStatusBadge;
use aurcache_client::Build;
use aurcache_common::build_state::BuildState;
use dioxus::prelude::*;

/// Columns that only appear once there is room for them.
const WIDE_ONLY: &str = "hidden md:table-cell";
/// Columns that wait for a laptop-sized screen: an unfolded phone has room for
/// everything else.
const WIDEST_ONLY: &str = "hidden lg:table-cell";

/// Every build, paged in the browser.
///
/// This asked for 100 with no way to reach a second page, so the history simply
/// stopped there and said nothing about it — and unlike the package list, a
/// build list only grows.
async fn load_builds() -> Result<Vec<Build>, String> {
    client()?
        .list_builds(&aurcache_client::BuildQuery::default())
        .await
        .map_err(|e| e.to_string())
}

#[component]
pub fn Builds(view: ViewParams, q: String) -> Element {
    let builds = use_resource(load_builds);

    // A build list only grows and its rows change state as work runs, so keep
    // it fresh on a timer — quick while a build is active or queued, a minute
    // otherwise — and re-fetch at once when an add enqueues new builds.
    let building = matches!(&*builds.read_unchecked(), Some(Ok(list))
        if list.iter().any(|b| Some(b.status).is_some_and(BuildState::is_in_progress)));
    crate::poll::use_poll(builds, building);
    crate::poll::use_refetch_on_package_change(builds);

    // Newest first: a build list is a log.
    const DEFAULT_SORT: Sort = Sort {
        key: SortKey::Time,
        dir: SortDir::Desc,
    };
    let status = use_signal(|| view.status_filter());
    let sort = use_signal(|| view.sort_or(DEFAULT_SORT));
    // Everything the URL carries is rebuilt from live state, so the term and
    // the controls write the same route rather than each dropping the other's
    // half.
    let to_route = move |q: String| Route::Builds {
        view: ViewParams::from_state(status(), sort(), DEFAULT_SORT, false),
        q,
    };
    let query = use_url_search(q, true, to_route);
    use_url_view(Some(query), true, to_route);
    let mut page = use_signal(|| 0usize);

    // The filter/sort/paginate pipeline, memoized: it re-runs only when its
    // inputs change (new data, new query/filter/sort/page), not on every
    // render a poll tick or a parent causes. The list only grows, so an
    // unmemoized render re-filters, re-sorts and re-clones all of it.
    let rows = use_memo(move || {
        let query = query();
        let status = status();
        let sort = sort();
        let page = page();
        match builds.read().as_ref() {
            None => Rows::Loading,
            Some(Err(e)) => Rows::Failed(e.clone()),
            Some(Ok(list)) => {
                let mut shown = filter_builds(list, &query, status);
                sort_builds(&mut shown, sort);
                Rows::Ready {
                    found: shown.len(),
                    total: list.len(),
                    current: paginate(&shown, page),
                }
            }
        }
    });

    // Changing what is listed puts you back at the start; see the same effect
    // on the packages screen.
    use_effect(use_reactive(&(query(), status()), move |_| page.set(0)));

    rsx! {
        div { class: "card bg-base-100 shadow-xl",
            div { class: "card-body",
                ListHeader { title: "Builds" }

                match &*rows.read() {
                    Rows::Loading => rsx! {
                        div { class: "flex justify-center p-8",
                            span { class: "loading loading-spinner loading-lg" }
                        }
                    },
                    Rows::Failed(e) => rsx! {
                        div { class: "alert alert-error", span { "Could not load builds: {e}" } }
                    },
                    Rows::Ready { total: 0, .. } => rsx! {
                        div { class: "alert", span { "No builds yet." } }
                    },
                    Rows::Ready { found, total, current } => {
                        rsx! {
                        ListControls {
                            query,
                            status,
                            placeholder: "Filter by package or build…",
                            shown: *found,
                            total: *total,
                            show_outdated: false,
                        }
                        if *found == 0 {
                            div { class: "alert mt-2", span { "Nothing matches that filter." } }
                        } else {
                        div { class: "overflow-x-auto",
                            table { class: LIST_TABLE,
                                thead {
                                    tr {
                                        // Sorting by build is sorting by
                                        // package: the package is the first
                                        // and most significant part of the
                                        // name now that there is no separate
                                        // column for it.
                                        SortableHeader { label: "Build", column: SortKey::Name, sort, class: "" }
                                        th { class: "{WIDE_ONLY}", "Version" }
                                        SortableHeader { label: "Started", column: SortKey::Time, sort, class: "{WIDE_ONLY}" }
                                        SortableHeader { label: "Duration", column: SortKey::Duration, sort, class: "{WIDE_ONLY}" }
                                        th { class: "{WIDEST_ONLY}", "Platform" }
                                        SortableHeader { label: "Worker", column: SortKey::Worker, sort, class: "{WIDE_ONLY}" }
                                        SortableHeader { label: "Size", column: SortKey::Size, sort, class: "{WIDE_ONLY} text-right" }
                                        SortableHeader { label: "Peak RAM", column: SortKey::Memory, sort, class: "{WIDEST_ONLY} text-right" }
                                        SortableHeader { label: "Status", column: SortKey::Status, sort, class: "" }
                                    }
                                }
                                tbody {
                                    for build in current.items.iter() {
                                        tr {
                                            key: "{build.pkg_name}/{build.number}",
                                            class: "hover cursor-pointer",
                                            // The row is the build: that is what
                                            // a build list is a list of.
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
                                            // A build's name already contains
                                            // its package, so a Package column
                                            // beside it said the same thing
                                            // twice. Every cell in the row now
                                            // leads to the same place, and the
                                            // package is one more click away,
                                            // from the build's own page.
                                            td {
                                                Link {
                                                    class: "font-mono [overflow-wrap:anywhere]",
                                                    to: Route::Build {
                                                        pkgbase: build.pkg_name.clone(),
                                                        number: build.number,
                                                    },
                                                    onclick: move |e: MouseEvent| e.stop_propagation(),
                                                    "{build.pkg_name}/{build.number}"
                                                }
                                                // What the dropped columns said,
                                                // on a screen too narrow for them.
                                                div { class: "md:hidden text-xs opacity-60",
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
                                            td { class: "{WIDEST_ONLY} text-sm opacity-70", "{build.platform}" }
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
                                                class: "{WIDEST_ONLY} text-right font-mono text-sm opacity-70",
                                                title: "Peak memory of the build's whole process tree, from its cgroup.",
                                                {build_peak_memory(build)}
                                            }
                                            td { BuildStatusBadge { status: build.status } }
                                        }
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
                            div { class: "text-sm opacity-60 pt-2", "{total} builds" }
                        }
                    }
                    },
                }
            }
        }
    }
}

/// The filter/sort/paginate pipeline's output, memoized in the component.
/// Comparable because [`use_memo`] demands it, which is also what skips a
/// re-render when a poll tick changes nothing.
#[derive(PartialEq)]
enum Rows {
    Loading,
    Failed(String),
    Ready {
        found: usize,
        total: usize,
        current: Page<Build>,
    },
}

/// A build's output size for the list column.
///
/// A dash for every build that produced nothing to measure -- failed, running,
/// or queued -- and for a successful build that predates the recording. The
/// status column beside it already says which.
///
/// Shared with the package builds list, which shows the same columns.
pub(crate) fn build_size(build: &Build) -> String {
    crate::format::format_size(build.size)
}

/// How much memory a build needed at its peak.
///
/// A dash where the worker reported nothing: an older worker, the deprecated
/// container builder, or a worker without a cgroup subtree to measure in. That
/// is a different statement from a build that used no memory, which cannot
/// happen -- so it is never rendered as `0 B`.
///
/// Shared with the package builds list, which shows the same columns.
pub(crate) fn build_peak_memory(build: &Build) -> String {
    crate::format::format_size(build.peak_memory)
}
