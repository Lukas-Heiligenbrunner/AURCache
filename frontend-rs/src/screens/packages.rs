//! The packages list.
//!
//! Shares `aurcache_client::SimplePackage` with the server rather than
//! redeclaring it, so a change to a response shape becomes a compile error
//! here instead of a silent drift between two hand-maintained models.

use crate::api::client;
use crate::listing::{
    LIST_TABLE, ListControls, ListHeader, Page, Pager, Sort, SortDir, SortKey, SortableHeader,
    ViewParams, filter_packages, paginate, sort_packages, use_url_search, use_url_view,
};
use crate::routes::Route;
use crate::status::StatusBadge;
use aurcache_client::SimplePackage;
use aurcache_common::build_state::BuildState;
use dioxus::prelude::*;

/// Columns that only appear once there is room for them.
const WIDE_ONLY: &str = "hidden md:table-cell";
/// Columns that wait for a laptop-sized screen: an unfolded phone has room for
/// everything else.
const WIDEST_ONLY: &str = "hidden lg:table-cell";

/// Everything, dependencies included, filtered down in the browser.
///
/// The toggle, the search and the status filter all narrow the same fetched
/// list, so switching any of them is immediate and the counts beside them stay
/// consistent. Fetching only what is shown would mean a round trip per toggle
/// and a "N of M" whose M is a page rather than a repository.
///
/// No limit, deliberately. This asked for 100 with no way to reach a second
/// page, so a repository larger than that silently showed a subset — and the
/// dependency closure is usually the larger part of one.
async fn load_packages() -> Result<Vec<SimplePackage>, String> {
    client()?
        .list_packages(&aurcache_client::PackageQuery {
            dependencies: aurcache_client::DependencySelection::Include,
            ..Default::default()
        })
        .await
        .map_err(|e| e.to_string())
}

#[component]
pub fn Packages(
    /// Filter and sort, carried in the query string.
    #[props(default)]
    view: ViewParams,
    q: String,
    /// Whether this list owns the URL's fragment.
    ///
    /// False behind the add dialog, which owns it there — two components
    /// writing the same fragment would fight, and the list would win by
    /// clearing the dialog's search on every keystroke.
    #[props(default = true)]
    sync_url: bool,
) -> Element {
    let mut packages = use_resource(load_packages);

    // Re-fetch on a timer so a build finishing, a version check or another
    // operator's change shows up without a manual reload; briskly while
    // anything here is still building. And immediately when an add lands,
    // rather than waiting out that timer — the dialog is a sibling of this
    // list, so it cannot restart the resource itself.
    let building = matches!(&*packages.read_unchecked(), Some(Ok(list))
        if list.iter().any(|p| Some(p.status).is_some_and(BuildState::is_in_progress)));
    crate::poll::use_poll(packages, building);
    crate::poll::use_refetch_on_package_change(packages);

    const DEFAULT_SORT: Sort = Sort {
        key: SortKey::Name,
        dir: SortDir::Asc,
    };
    let status = use_signal(|| view.status_filter());
    // Off by default: the list reads as the set of packages somebody is
    // maintaining, and a dependency closure buries that under packages nobody
    // chose. The URL can ask for them.
    let mut show_dependencies = use_signal(|| view.dependencies);
    let sort = use_signal(|| view.sort_or(DEFAULT_SORT));

    // Rebuilt from live state, so the term and the controls write one route
    // between them instead of each dropping the other's half.
    let to_route = move |q: String| Route::Packages {
        view: ViewParams::from_state(status(), sort(), DEFAULT_SORT, show_dependencies()),
        q,
    };
    let query = use_url_search(q, sync_url, to_route);
    use_url_view(Some(query), sync_url, to_route);
    let mut page = use_signal(|| 0usize);

    // The scope/filter/sort/paginate pipeline, memoized like the builds
    // screen's: it re-runs only when its inputs change, not on every render
    // a poll tick or a parent causes.
    let rows = use_memo(move || {
        let query = query();
        let status = status();
        let sort = sort();
        let page = page();
        let show_all = show_dependencies();
        match packages.read().as_ref() {
            None => Rows::Loading,
            Some(Err(e)) => Rows::Failed(e.clone()),
            Some(Ok(list)) => {
                let dependencies = list.iter().filter(|p| !p.directly_requested).count();
                // The toggle narrows first, so `total` is the size of the
                // list being searched rather than of the fetch. Otherwise the
                // count beside the search box would report packages the page
                // is not showing.
                let total = if show_all {
                    list.len()
                } else {
                    list.len() - dependencies
                };
                let in_scope = list.iter().filter(|pkg| show_all || pkg.directly_requested);
                let mut shown = filter_packages(in_scope, &query, status);
                sort_packages(&mut shown, sort);
                // Sorted first, so a page is a slice of the order on screen
                // rather than of the order it arrived in.
                Rows::Ready {
                    found: shown.len(),
                    total,
                    dependencies,
                    current: paginate(&shown, page),
                }
            }
        }
    });

    // Anything that changes which rows exist puts you back at the start.
    // Without this, narrowing a filter while on page 3 lands on a page that no
    // longer has anything on it — `paginate` clamps so it is not blank, but
    // arriving mid-list after typing a filter is still not what was asked for.
    use_effect(use_reactive(
        &(query(), status(), show_dependencies()),
        move |_| page.set(0),
    ));

    rsx! {
        div { class: "card bg-base-100 shadow-xl",
            div { class: "card-body",
                ListHeader { title: "Packages",
                    Link { class: "btn btn-primary btn-sm", to: Route::PackageAdd { q: String::new() },
                        "Add package"
                    }
                }

                match &*rows.read() {
                    Rows::Loading => rsx! {
                        div { class: "flex justify-center p-8",
                            span { class: "loading loading-spinner loading-lg" }
                        }
                    },
                    Rows::Failed(e) => rsx! {
                        div { class: "alert alert-error",
                            span { "Could not load packages: {e}" }
                        }
                    },
                    // Empty fetch, not an empty scope: with the toggle off and
                    // only dependencies fetched, `total` is 0 but there are
                    // packages, so that case reads "nothing matches" below.
                    Rows::Ready { total: 0, dependencies: 0, .. } => rsx! {
                        div { class: "alert", span { "No packages yet." } }
                    },
                    Rows::Ready { found, total, dependencies, current } => {
                        rsx! {
                        ListControls {
                            query,
                            status,
                            placeholder: "Filter packages…",
                            shown: *found,
                            total: *total,
                            show_outdated: true,
                            // Sits beside the status filter. Only when there
                            // are dependencies to reveal — a checkbox that
                            // changes nothing invites the reader to wonder
                            // what it is for.
                            if *dependencies > 0 {
                                label { class: "label cursor-pointer gap-2 py-0",
                                    input {
                                        r#type: "checkbox",
                                        class: "checkbox checkbox-sm",
                                        checked: show_dependencies(),
                                        onchange: move |e| show_dependencies.set(e.checked()),
                                    }
                                    span { class: "label-text text-sm whitespace-nowrap",
                                        "Dependencies ({dependencies})"
                                    }
                                }
                            }
                        }
                        if *found == 0 {
                            div { class: "alert mt-2", span { "Nothing matches that filter." } }
                        } else {
                        div { class: "overflow-x-auto",
                            table { class: LIST_TABLE,
                                thead {
                                    tr {
                                        SortableHeader { label: "Package", column: SortKey::Name, sort, class: "" }
                                        // On a narrow screen the version sits
                                        // under the name instead, and Upstream,
                                        // Size and Actions are dropped rather
                                        // than scrolled to.
                                        th { class: "{WIDE_ONLY}", "Version" }
                                        th { class: "{WIDEST_ONLY}", "Upstream" }
                                        SortableHeader { label: "Size", column: SortKey::Size, sort, class: "{WIDE_ONLY} text-right" }
                                        SortableHeader { label: "Status", column: SortKey::Status, sort, class: "" }
                                        th { class: "{WIDE_ONLY} text-right", "Actions" }
                                    }
                                }
                                tbody {
                                    for pkg in current.items.iter() {
                                        tr {
                                            key: "{pkg.name}",
                                            class: "hover cursor-pointer",
                                            // The whole row is the target, but
                                            // the name stays a real link so the
                                            // address is copyable, middle-click
                                            // opens a tab, and keyboard users
                                            // have something to focus — none of
                                            // which a bare row handler gives.
                                            onclick: {
                                                let pkgbase = pkg.name.clone();
                                                move |_| {
                                                    navigator().push(Route::Package {
                                                        pkgbase: pkgbase.clone(),
                                                    });
                                                }
                                            },
                                            td {
                                                Link {
                                                    // Monospace, matching the
                                                    // package page's heading:
                                                    // a pkgbase is an identifier
                                                    // and reads as one.
                                                    //
                                                    // Not `link link-primary`:
                                                    // when everything in the row
                                                    // navigates, underlining one
                                                    // cell implies the rest does
                                                    // not.
                                                    // `anywhere` lets the column shrink
                                                    // below a long name on a phone; a
                                                    // name that fits is not broken.
                                                    class: "font-mono [overflow-wrap:anywhere]",
                                                    // pkgbase is the public identifier.
                                                    to: Route::Package { pkgbase: pkg.name.clone() },
                                                    // Otherwise the click reaches
                                                    // the row too and pushes the
                                                    // same route twice, leaving a
                                                    // duplicate history entry.
                                                    onclick: move |e: MouseEvent| e.stop_propagation(),
                                                    "{pkg.name}"
                                                }
                                                // Only meaningful while both
                                                // kinds are on screen; with the
                                                // toggle off every row would
                                                // carry the opposite of it.
                                                if !pkg.directly_requested {
                                                    span {
                                                        class: "badge badge-outline badge-sm ml-2",
                                                        title: "Pulled in as a dependency, not requested directly",
                                                        "dependency"
                                                    }
                                                }
                                                div { class: "md:hidden font-mono text-xs opacity-60 [overflow-wrap:anywhere]",
                                                    {pkg.built_version.clone().unwrap_or_else(|| "—".into())}
                                                }
                                            }
                                            td { class: "{WIDE_ONLY} font-mono text-sm",
                                                {pkg.built_version.clone().unwrap_or_else(|| "—".into())}
                                            }
                                            td { class: "{WIDEST_ONLY} font-mono text-sm opacity-70",
                                                {pkg.upstream_version.clone().unwrap_or_else(|| "—".into())}
                                            }
                                            td { class: "{WIDE_ONLY} text-right font-mono text-sm opacity-70 whitespace-nowrap",
                                                {package_size(pkg)}
                                            }
                                            td { StatusBadge { status: pkg.status, out_of_date: pkg.out_of_date } }
                                            td { class: "{WIDE_ONLY} text-right",
                                                RowAction {
                                                    pkgbase: pkg.name.clone(),
                                                    action: row_action(pkg.status, pkg.out_of_date),
                                                    on_changed: move |()| packages.restart(),
                                                }
                                            }
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
                        // The pager already says "of N" once there is more
                        // than one page; this is for the case where there is
                        // not, so a short list still says how short.
                        if current.pages <= 1 {
                            div { class: "text-sm opacity-60 pt-2", "{total} packages" }
                        }
                    }
                    },
                }
            }
        }
    }
}

/// The scope/filter/sort/paginate pipeline's output, memoized in the component.
/// Same shape as the builds screen's, plus the dependency count the toggle's
/// checkbox carries. Comparable because [`use_memo`] demands it.
#[derive(PartialEq)]
enum Rows {
    Loading,
    Failed(String),
    Ready {
        found: usize,
        total: usize,
        dependencies: usize,
        current: Page<SimplePackage>,
    },
}

/// A package's combined artifact size for the list column.
///
/// A dash covers both "nothing built yet" and "a size is missing" — the server
/// sends `None` for either, and the status column already says which of the two
/// this row is.
fn package_size(pkg: &SimplePackage) -> String {
    crate::format::format_size(pkg.total_size)
}

/// What a package's row offers to do about its current state.
///
/// One button, whose label is the thing it would actually accomplish. A row
/// that always says "Update" says it of a package that is up to date, of one
/// whose last build failed, and of one already queued -- three states in which
/// it means three different things, and one in which it means nothing at all.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    /// Upstream has moved: fetch the new version.
    Update,
    /// The last build failed. Same version, another go.
    Retry,
    /// Nothing is wrong; build it again anyway.
    Rebuild,
}

impl Action {
    fn label(self) -> &'static str {
        match self {
            Self::Update => "Update",
            Self::Retry => "Retry",
            Self::Rebuild => "Rebuild",
        }
    }

    /// Whether to build regardless of what the upstream version says.
    ///
    /// Only `Update` has a new version to go and get; the other two are asking
    /// for another build of what is already there, which an unforced update
    /// would decline to do.
    fn force(self) -> bool {
        self != Self::Update
    }
}

/// The action a package's state calls for, or `None` when there is nothing
/// useful to offer.
///
/// Out of date comes before the build outcome: a package whose last build
/// failed *and* which has since gone out of date is better served by fetching
/// the new version than by rebuilding the one that failed.
///
/// A package with a build already queued or running gets nothing. The work is
/// happening; a second request would either be refused or queue a duplicate,
/// and a button cannot say which.
#[must_use]
pub fn row_action(status: BuildState, out_of_date: bool) -> Option<Action> {
    match status {
        BuildState::Active
        | BuildState::Enqueued
        | BuildState::WaitingForDeps
        | BuildState::Publishing => None,
        _ if out_of_date => Some(Action::Update),
        BuildState::Failed => Some(Action::Retry),
        BuildState::Successful => Some(Action::Rebuild),
    }
}

/// The row's button, or nothing.
#[component]
fn RowAction(pkgbase: String, action: Option<Action>, on_changed: EventHandler<()>) -> Element {
    let mut busy = use_signal(|| false);
    let mut error = use_signal(|| Option::<String>::None);

    let Some(action) = action else {
        return rsx! {};
    };

    rsx! {
        div { class: "flex items-center justify-end gap-2",
            if let Some(message) = error() {
                span { class: "text-xs text-error", title: "{message}", "failed" }
            }
            button {
                // `relative`, because the spinner below is positioned over the
                // label rather than laid out beside it. Adding it to the flow
                // widened the button mid-click, which pushed the whole column
                // sideways and shifted the status badges of every row -- a list
                // that moves while you are clicking it.
                class: "btn btn-ghost btn-xs relative",
                disabled: busy(),
                // A button inside a clickable row has to claim its own click,
                // or pressing it also navigates away.
                onclick: move |e: MouseEvent| {
                    e.stop_propagation();
                    let pkgbase = pkgbase.clone();
                    async move {
                        busy.set(true);
                        error.set(None);
                        let outcome = match client() {
                            Ok(client) => client
                                .update_package(
                                    &pkgbase,
                                    &aurcache_client::UpdatePackage { force: action.force() },
                                )
                                .await
                                .map_err(|e| e.to_string()),
                            Err(e) => Err(e),
                        };
                        match outcome {
                            // The list is how the new state becomes visible:
                            // the row's action changes as soon as the build is
                            // queued, which is the feedback that it worked.
                            Ok(_) => on_changed.call(()),
                            Err(e) => error.set(Some(e)),
                        }
                        busy.set(false);
                    }
                },
                if busy() {
                    span { class: "loading loading-spinner loading-xs absolute inset-0 m-auto" }
                }
                // Hidden rather than removed: it still occupies its space, so
                // the button stays exactly as wide as its label whatever it is
                // doing. That holds for any label without a reserved width to
                // keep in step with the longest one.
                span { class: if busy() { "invisible" } else { "" }, {action.label()} }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Action, row_action};
    use aurcache_common::build_state::BuildState;

    const FRESH: bool = false;
    const STALE: bool = true;

    /// The four states the button is meant to distinguish, which one label for
    /// all of them could not.
    #[test]
    fn the_action_matches_what_the_package_needs() {
        assert_eq!(
            row_action(BuildState::Successful, FRESH),
            Some(Action::Rebuild)
        );
        assert_eq!(row_action(BuildState::Failed, FRESH), Some(Action::Retry));
        assert_eq!(
            row_action(BuildState::Successful, STALE),
            Some(Action::Update)
        );
    }

    /// Work already in flight. A second request would either be refused or
    /// queue a duplicate, and the row cannot say which.
    #[test]
    fn a_package_already_building_offers_nothing() {
        for state in [
            BuildState::Active,
            BuildState::Enqueued,
            BuildState::WaitingForDeps,
            BuildState::Publishing,
        ] {
            assert_eq!(row_action(state, FRESH), None, "{state:?}");
            // Even out of date: the build under way is what resolves it.
            assert_eq!(row_action(state, STALE), None, "{state:?}");
        }
    }

    /// A new version is worth more than another go at the old one, so this
    /// says Update rather than Retry.
    #[test]
    fn out_of_date_outranks_a_failed_build() {
        assert_eq!(row_action(BuildState::Failed, STALE), Some(Action::Update));
    }

    /// Only `Update` has a new version to fetch; the others are asking for
    /// another build of what is there, which an unforced update declines.
    #[test]
    fn only_an_update_defers_to_the_upstream_version() {
        assert!(!Action::Update.force());
        assert!(Action::Retry.force());
        assert!(Action::Rebuild.force());
    }
}
