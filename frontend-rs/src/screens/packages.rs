//! The packages list.
//!
//! Shares `aurcache_client::SimplePackage` with the server rather than
//! redeclaring it, so a change to a response shape becomes a compile error
//! here instead of a silent drift between two hand-maintained models.

use crate::action::use_action;
use crate::api::client;
use crate::format::format_size;
use crate::list_controls::{
    LIST_TABLE, ListControls, ListHeader, Pager, Rows, Selection, SortableHeader, WIDE_ONLY,
    WIDEST_ONLY, use_rows, use_url_search, use_url_view,
};
use crate::listing::{Sort, SortDir, SortKey, ViewParams, filter_packages, sort_packages};
use crate::routes::Route;
use crate::status::StatusBadge;
use crate::widgets::{LinkRow, LoadFailed, RowLink, Spinner};
use aurcache_client::SimplePackage;
use aurcache_common::build_state::BuildState;
use dioxus::prelude::*;

/// Everything, dependencies included, filtered down in the browser.
///
/// The toggle, the search and the status filter all narrow the same fetched
/// list, so switching any of them is immediate and the counts beside them stay
/// consistent. Fetching only what is shown would mean a round trip per toggle
/// and a "N of M" whose M is a page rather than a repository.
///
/// No limit, deliberately: a capped fetch would silently show a subset, and the
/// dependency closure is usually the larger part of a repository.
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
        if list.iter().any(|p| p.status.is_in_progress()));
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
    use_url_view(sync_url, move || to_route(query.peek().clone()));
    let mut page = use_signal(|| 0usize);

    let rows = use_rows(packages, page, move |list| {
        let show_all = show_dependencies();
        let in_scope = list.iter().filter(|pkg| show_all || pkg.directly_requested);
        // The toggle narrows first, so `total` is the size of the list being
        // searched rather than of the fetch. Otherwise the count beside the
        // search box would report packages the page is not showing.
        let total = in_scope.clone().count();
        let mut shown = filter_packages(in_scope, &query(), status());
        sort_packages(&mut shown, sort());
        Selection { shown, total }
    });
    // What the toggle would reveal, and whether to offer it at all.
    let dependencies = use_memo(move || match packages.read().as_ref() {
        Some(Ok(list)) => list.iter().filter(|p| !p.directly_requested).count(),
        _ => 0,
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
                    Rows::Loading => rsx! { Spinner {} },
                    Rows::Failed(e) => rsx! { LoadFailed { what: "packages", error: e.clone() } },
                    // Empty fetch, not an empty scope: with the toggle off and
                    // only dependencies fetched, `total` is 0 but there are
                    // packages, so that case reads "nothing matches" below.
                    Rows::Ready { total: 0, .. } if dependencies() == 0 => rsx! {
                        div { class: "alert", span { "No packages yet." } }
                    },
                    Rows::Ready { found, total, current } => {
                        let dependencies = dependencies();
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
                            if dependencies > 0 {
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
                                        LinkRow {
                                            key: "{pkg.name}",
                                            // pkgbase is the public identifier.
                                            to: Route::Package { pkgbase: pkg.name.clone() },
                                            td {
                                                RowLink {
                                                    // Monospace, matching the
                                                    // package page's heading:
                                                    // a pkgbase is an identifier
                                                    // and reads as one.
                                                    // `anywhere` lets the column
                                                    // shrink below a long name on
                                                    // a phone; a name that fits
                                                    // is not broken.
                                                    class: "font-mono [overflow-wrap:anywhere]",
                                                    to: Route::Package { pkgbase: pkg.name.clone() },
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
                                                // A dash covers both "nothing
                                                // built yet" and "a size is
                                                // missing"; the status column
                                                // says which.
                                                {format_size(pkg.total_size)}
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
                        Pager { page, info: current.info }
                        // The pager already says "of N" once there is more
                        // than one page; this is for the case where there is
                        // not, so a short list still says how short.
                        if current.info.pages <= 1 {
                            div { class: "text-sm opacity-60 pt-2", "{total} packages" }
                        }
                    }
                    },
                }
            }
        }
    }
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
    let request = use_action();

    let Some(action) = action else {
        return rsx! {};
    };

    rsx! {
        div { class: "flex items-center justify-end gap-2",
            if let Some(message) = request.error() {
                span { class: "text-xs text-error", title: "{message}", "failed" }
            }
            button {
                // `relative`, because the spinner below is positioned over the
                // label rather than laid out beside it. Adding it to the flow
                // widened the button mid-click, which pushed the whole column
                // sideways and shifted the status badges of every row -- a list
                // that moves while you are clicking it.
                class: "btn btn-ghost btn-xs relative",
                disabled: request.busy(),
                // A button inside a clickable row has to claim its own click,
                // or pressing it also navigates away.
                onclick: move |e: MouseEvent| {
                    e.stop_propagation();
                    let pkgbase = pkgbase.clone();
                    async move {
                        let queued = request
                            .run(async {
                                client()?
                                    .update_package(
                                        &pkgbase,
                                        &aurcache_client::UpdatePackage { force: action.force() },
                                    )
                                    .await
                                    .map_err(|e| e.to_string())
                            })
                            .await;
                        // The list is how the new state becomes visible: the
                        // row's action changes as soon as the build is queued,
                        // which is the feedback that it worked.
                        if queued.is_some() {
                            on_changed.call(());
                        }
                    }
                },
                if request.busy() {
                    span { class: "loading loading-spinner loading-xs absolute inset-0 m-auto" }
                }
                // Hidden rather than removed: it still occupies its space, so
                // the button stays exactly as wide as its label whatever it is
                // doing. That holds for any label without a reserved width to
                // keep in step with the longest one.
                span { class: if request.busy() { "invisible" } else { "" }, {action.label()} }
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
