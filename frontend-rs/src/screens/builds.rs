//! The builds list.

use crate::api::client;
use crate::dates::DateOnly;
use crate::format::{format_duration, format_size};
use crate::list_controls::{
    LIST_TABLE, ListControls, ListHeader, Pager, Rows, Selection, SortableHeader, WIDE_ONLY,
    WIDEST_ONLY, use_rows, use_url_search, use_url_view,
};
use crate::listing::{PageInfo, Sort, SortDir, SortKey, ViewParams, filter_builds, sort_builds};
use crate::routes::Route;
use crate::status::BuildStatusBadge;
use crate::widgets::{LinkRow, RowLink, Spinner};
use aurcache_client::Build;
use dioxus::prelude::*;

/// Every build, paged in the browser: a build list only grows, so a capped
/// fetch would end the history silently.
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
        if list.iter().any(|b| b.status.is_in_progress()));
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
    use_url_view(true, move || to_route(query.peek().clone()));
    let mut page = use_signal(|| 0usize);

    let rows = use_rows(builds, page, move |list| {
        let mut shown = filter_builds(list, &query(), status());
        sort_builds(&mut shown, sort());
        Selection {
            shown,
            total: list.len(),
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
                    Rows::Loading => rsx! { Spinner {} },
                    Rows::Failed(e) => rsx! {
                        div { class: "alert alert-error", span { "Could not load builds: {e}" } }
                    },
                    Rows::Ready { total: 0, .. } => rsx! {
                        div { class: "alert", span { "No builds yet." } }
                    },
                    Rows::Ready { found, total, current } => rsx! {
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
                            BuildTable {
                                builds: current.items.clone(),
                                info: current.info,
                                total: *total,
                                page,
                                sort,
                                of: BuildsOf::EveryPackage,
                            }
                        }
                    },
                }
            }
        }
    }
}

/// Whose builds a [`BuildTable`] lists, which decides what each row has to say.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum BuildsOf {
    /// Every package's: a row names its package.
    EveryPackage,
    /// One package's, under its header: the package goes without saying.
    OnePackage,
}

impl BuildsOf {
    /// How a row names its build.
    ///
    /// A build's name already contains its package, so a Package column beside
    /// it would say the same thing twice. Sorting by build is then sorting by
    /// package, the first and most significant part of the name.
    fn name(self, build: &Build) -> String {
        match self {
            Self::EveryPackage => format!("{}/{}", build.pkg_name, build.number),
            Self::OnePackage => build.number.to_string(),
        }
    }

    /// Where the columns a reader needs least go once the screen narrows.
    ///
    /// One package's builds share their platform and rarely differ much in
    /// memory, so those columns are dropped with the rest there; across every
    /// package they wait for a laptop-sized screen instead.
    fn least_needed(self) -> &'static str {
        match self {
            Self::EveryPackage => WIDEST_ONLY,
            Self::OnePackage => WIDE_ONLY,
        }
    }
}

/// One page of builds as a table, with its pager and count.
#[component]
pub(crate) fn BuildTable(
    builds: Vec<Build>,
    info: PageInfo,
    /// How many builds there are before any filter, for the count under a
    /// single page.
    total: usize,
    page: Signal<usize>,
    sort: Signal<Sort>,
    of: BuildsOf,
) -> Element {
    let rare = of.least_needed();

    rsx! {
        div { class: "overflow-x-auto",
            table { class: LIST_TABLE,
                thead {
                    tr {
                        SortableHeader { label: "Build", column: SortKey::Name, sort, class: "" }
                        th { class: "{WIDE_ONLY}", "Version" }
                        SortableHeader { label: "Started", column: SortKey::Time, sort, class: "{WIDE_ONLY}" }
                        SortableHeader { label: "Duration", column: SortKey::Duration, sort, class: "{WIDE_ONLY}" }
                        th { class: "{rare}", "Platform" }
                        SortableHeader { label: "Worker", column: SortKey::Worker, sort, class: "{WIDE_ONLY}" }
                        SortableHeader { label: "Size", column: SortKey::Size, sort, class: "{WIDE_ONLY} text-right" }
                        SortableHeader { label: "Peak RAM", column: SortKey::Memory, sort, class: "{rare} text-right" }
                        SortableHeader { label: "Status", column: SortKey::Status, sort, class: "" }
                    }
                }
                tbody {
                    for build in builds.iter() {
                        LinkRow {
                            key: "{build.pkg_name}/{build.number}",
                            // The row is the build: that is what a build list
                            // is a list of.
                            to: Route::Build {
                                pkgbase: build.pkg_name.clone(),
                                number: build.number,
                            },
                            td {
                                RowLink {
                                    class: "font-mono [overflow-wrap:anywhere]",
                                    to: Route::Build {
                                        pkgbase: build.pkg_name.clone(),
                                        number: build.number,
                                    },
                                    "{of.name(build)}"
                                }
                                // What the dropped columns said, on a screen
                                // too narrow for them.
                                div { class: "md:hidden text-xs opacity-60 [overflow-wrap:anywhere]",
                                    if of == BuildsOf::OnePackage {
                                        span { class: "font-mono", "{build.version}" }
                                        " · "
                                    }
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
                            td { class: "{rare} text-sm opacity-70", "{build.platform}" }
                            td { class: "{WIDE_ONLY} text-sm opacity-70",
                                if let Some(worker) = build.worker_name.as_deref() {
                                    "{worker}"
                                } else {
                                    // Not "no worker": nobody has claimed it
                                    // yet.
                                    span {
                                        class: "opacity-40",
                                        title: "Not claimed by a worker yet.",
                                        "—"
                                    }
                                }
                            }
                            // A dash for every build that produced nothing to
                            // measure -- failed, running or queued -- which the
                            // status column beside it already explains.
                            td { class: "{WIDE_ONLY} text-right font-mono text-sm opacity-70",
                                {format_size(build.size)}
                            }
                            // A dash where nothing was reported -- a worker
                            // without a cgroup subtree to measure in -- which
                            // is not a build that used no memory.
                            td {
                                class: "{rare} text-right font-mono text-sm opacity-70",
                                title: "Peak memory of the build's whole process tree, from its cgroup.",
                                {format_size(build.peak_memory)}
                            }
                            td { BuildStatusBadge { status: build.status } }
                        }
                    }
                }
            }
        }
        Pager { page, info }
        // The pager already says "of N" once there is more than one page; this
        // is for the case where there is not, so a short list still says how
        // short.
        if info.pages <= 1 {
            div { class: "text-sm opacity-60 pt-2", "{total} builds" }
        }
    }
}
