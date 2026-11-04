//! The controls the lists share: header, search box, status filter, sortable
//! columns, pager, and the hooks that keep them in the URL.
//!
//! The behaviour behind them -- filtering, sorting, paging, the URL encoding --
//! is in `crate::listing`, as pure functions; this is only what renders it.

use crate::listing::{
    Page, PageInfo, Sort, SortDir, SortKey, StatusFilter, filter_summary, paginate, state_label,
    states_by_attention,
};
use crate::routes::Route;
use dioxus::prelude::*;

/// A list's table. daisyUI's cell padding spends a fifth of a phone's width on
/// two columns, so it is halved below `md`.
pub const LIST_TABLE: &str = "table table-zebra max-md:[&_td]:px-2 max-md:[&_th]:px-2";

/// Columns that only appear once there is room for them.
pub const WIDE_ONLY: &str = "hidden md:table-cell";

/// Columns that wait for a laptop-sized screen: an unfolded phone has room for
/// everything else.
pub const WIDEST_ONLY: &str = "hidden lg:table-cell";

/// A list card's title, with an optional action on the right.
///
/// Shared so the lists cannot drift: `min-h-8` matches a `btn-sm`, so the
/// header occupies the same height whether or not it has an action.
#[component]
pub fn ListHeader(title: String, children: Element) -> Element {
    rsx! {
        div { class: "flex items-center gap-2 min-h-8",
            h2 { class: "card-title", "{title}" }
            div { class: "flex-1" }
            {children}
        }
    }
}

/// How long the box has to be still before the address bar follows it.
///
/// The list filters on every keystroke; only the URL waits. Browsers rate-limit
/// history writes — Safari at roughly a hundred per thirty seconds — and typing
/// a word is easily a dozen.
const URL_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(300);

/// A search term that lives in the URL fragment, so a search can be linked to.
///
/// Seeded from the route on the way in and written back as it changes, always
/// with `replace` rather than `push`: one history entry per keystroke would turn
/// Back into a backspace.
///
/// `to_route` builds the route for a given term, which is all that differs
/// between the pages doing this — keeping it in one place is what stops the
/// fragment meaning something different on each.
///
/// `sync` is a parameter rather than the caller skipping this hook, because a
/// hook that is sometimes called breaks the order Dioxus identifies them by.
/// The packages list passes `false` when it is rendered behind the add dialog,
/// which owns the fragment there.
pub fn use_url_search(
    initial: String,
    sync: bool,
    // Not a `fn` pointer: the builds list has a status filter to carry through
    // the same URL, so the route a term maps to depends on more than the term.
    to_route: impl Fn(String) -> Route + Clone + 'static,
) -> Signal<String> {
    let term = use_signal(|| initial);

    // A timer per keystroke rather than one writer looping for the life of the
    // screen: an idle list then costs nothing, where a loop would wake every
    // debounce interval whether or not anything was typed.
    use_effect(move || {
        if !sync {
            return;
        }
        // Reads `term`, so it re-runs when the box changes and not on every
        // unrelated render.
        let value = term();
        let to_route = to_route.clone();
        spawn(async move {
            gloo_timers::future::sleep(URL_DEBOUNCE).await;
            // Only the last keystroke of a burst writes: the earlier timers find
            // the box has moved on and do nothing.
            if *term.peek() == value {
                navigator().replace(to_route(value));
            }
        });
    });

    term
}

/// Search box and status filter, shared by both lists.
///
/// `children` is rendered inline after the status filter, for a list that has
/// one more control to sit beside it — the packages list puts its "show
/// dependencies" toggle there. Nothing passes it on the builds list.
#[component]
pub fn ListControls(
    query: Signal<String>,
    status: Signal<StatusFilter>,
    placeholder: String,
    shown: usize,
    total: usize,
    /// Whether the "Out of date" pseudo-status is offered. Packages have the
    /// backing flag; builds do not.
    show_outdated: bool,
    children: Element,
) -> Element {
    let mut query = query;
    let mut status = status;

    rsx! {
        div { class: "flex flex-wrap items-center gap-2",
            input {
                class: "input input-bordered input-sm w-full sm:w-64",
                r#type: "search",
                placeholder,
                value: "{query}",
                aria_label: "Filter by name",
                oninput: move |e| query.set(e.value()),
            }
            div { class: "dropdown",
                div {
                    tabindex: "0",
                    role: "button",
                    class: "btn btn-sm btn-outline",
                    aria_label: "Filter by status",
                    "{filter_summary(status())}"
                }
                ul {
                    tabindex: "0",
                    class: "dropdown-content menu bg-base-100 rounded-box z-[1] w-56 p-2 shadow",
                    if show_outdated {
                        li {
                            label { class: "label cursor-pointer gap-2",
                                input {
                                    r#type: "checkbox",
                                    class: "checkbox checkbox-sm",
                                    checked: status().out_of_date,
                                    onchange: move |_| status.set(status().toggled_outdated()),
                                }
                                span { class: "label-text", "Out of date" }
                            }
                        }
                    }
                    for state in states_by_attention() {
                        li { key: "{state.as_i32()}",
                            label { class: "label cursor-pointer gap-2",
                                input {
                                    r#type: "checkbox",
                                    class: "checkbox checkbox-sm",
                                    checked: status().states.contains(state),
                                    onchange: move |_| status.set(status().toggled_state(state)),
                                }
                                span { class: "label-text", "{state_label(state)}" }
                            }
                        }
                    }
                }
            }
            {children}
            div { class: "flex-1" }
            // Only worth saying when a filter is actually hiding something.
            if shown != total {
                span { class: "text-sm opacity-60", "{shown} of {total}" }
            }
        }
    }
}

/// A column header that sorts the list when clicked.
#[component]
pub fn SortableHeader(
    label: String,
    column: SortKey,
    sort: Signal<Sort>,
    class: String,
) -> Element {
    let mut sort = sort;
    let active = sort().key == column;
    // Conveys the ordering to assistive tech, which the arrow only shows
    // visually.
    let aria_sort = if active {
        match sort().dir {
            SortDir::Asc => "ascending",
            SortDir::Desc => "descending",
        }
    } else {
        "none"
    };

    rsx! {
        th { class,
            button {
                // `inline-flex`, not `flex`: a block-level flex container fills
                // the cell and packs its label at the left, so a `text-right`
                // header (Size) sorted the same as the values below it but did
                // not line up with them. Inline-level means the cell's own
                // text-align places it.
                class: "inline-flex items-center gap-1 hover:opacity-100 opacity-90",
                onclick: move |_| sort.set(sort().toggled(column)),
                aria_sort,
                "{label}"
                if active {
                    span { class: "text-xs opacity-70", "{sort().dir.arrow()}" }
                }
            }
        }
    }
}

/// What a list shows: still loading, failed, or one page of what survived its
/// filters. Comparable because [`use_memo`] demands it, which is also what
/// skips a re-render when a poll tick changes nothing.
#[derive(PartialEq)]
pub enum Rows<T> {
    Loading,
    Failed(String),
    Ready {
        /// How many rows survived the filters, across every page.
        found: usize,
        /// How many rows there are to filter.
        total: usize,
        current: Page<T>,
    },
}

/// The rows a list's filters leave, and how many there were to filter.
pub struct Selection<T> {
    pub shown: Vec<T>,
    pub total: usize,
}

/// A list's filter, sort and paginate pipeline, memoized: it re-runs only when
/// its inputs change -- new data, or a signal `select` or `page` reads -- not on
/// every render a poll tick or a parent causes.
///
/// `select` filters and sorts what was fetched. Paging comes after it, so a page
/// is a slice of the order on screen rather than of the order it arrived in.
pub fn use_rows<T: Clone + PartialEq + 'static>(
    list: Resource<Result<Vec<T>, String>>,
    page: Signal<usize>,
    select: impl Fn(&[T]) -> Selection<T> + 'static,
) -> Memo<Rows<T>> {
    use_memo(move || match list.read().as_ref() {
        None => Rows::Loading,
        Some(Err(e)) => Rows::Failed(e.clone()),
        Some(Ok(list)) => {
            let Selection { shown, total } = select(list);
            Rows::Ready {
                found: shown.len(),
                total,
                current: paginate(&shown, page()),
            }
        }
    })
}

/// Keep the URL in step with the filter and sort controls.
///
/// `replace`, never `push`, for the same reason the search box uses it: a
/// person narrowing a list is refining one view, not walking a trail, and Back
/// should leave the list rather than undo a dropdown one notch at a time. That
/// is also why this is one hook rather than one per control -- every change
/// rewrites the whole route from live state, so the term and the controls
/// cannot overwrite each other's half of the URL.
///
/// `to_route` builds the whole route from the control signals, so this only has
/// to fire when one of them changes. A list with a search box reads its term
/// with `peek` there: the box has its own debounced writer, and subscribing
/// here would race it.
///
/// `sync` matches `use_url_search`: the packages list passes `false` behind the
/// add dialog, which owns the URL there. A parameter rather than the caller
/// skipping the hook, because Dioxus identifies hooks by order.
pub fn use_url_view(sync: bool, to_route: impl Fn() -> Route + Clone + 'static) {
    // Seeded with the route we arrived on, *not* `None`. Starting empty made
    // the first effect run navigate to the URL the page was already showing;
    // that re-entered the router on mount and the app intermittently failed to
    // come up at all. Nothing is written until a control actually moves.
    let seed = to_route.clone();
    let mut previous = use_signal(seed);
    use_effect(move || {
        if !sync {
            return;
        }
        let route = to_route();
        // Unchanged means an unrelated render rather than a control moving, and
        // replacing again would be a wasted navigation.
        if *previous.peek() == route {
            return;
        }
        previous.set(route.clone());
        navigator().replace(route);
    });
}

/// Page controls, shown only when there is more than one page.
#[component]
pub fn Pager(page: Signal<usize>, info: PageInfo) -> Element {
    let mut page = page;
    let PageInfo {
        index,
        pages,
        first,
        count,
        total,
    } = info;
    if pages <= 1 {
        return rsx! {};
    }
    let (from, to) = (first + 1, first + count);

    rsx! {
        div { class: "flex flex-wrap items-center gap-2 mt-2",
            span { class: "text-sm opacity-60", "Showing {from}\u{2013}{to} of {total}" }
            div { class: "flex-1" }
            div { class: "join",
                button {
                    class: "btn btn-sm join-item",
                    disabled: index == 0,
                    // Not `page -= 1`: the rendered page is clamped, so the
                    // signal can be further along than what is on screen, and
                    // stepping back from the stale value would appear to do
                    // nothing.
                    onclick: move |_| page.set(index.saturating_sub(1)),
                    aria_label: "Previous page",
                    "\u{2039}"
                }
                button { class: "btn btn-sm join-item no-animation", disabled: true,
                    "Page {index + 1} of {pages}"
                }
                button {
                    class: "btn btn-sm join-item",
                    disabled: index + 1 >= pages,
                    onclick: move |_| page.set(index + 1),
                    aria_label: "Next page",
                    "\u{203a}"
                }
            }
        }
    }
}
