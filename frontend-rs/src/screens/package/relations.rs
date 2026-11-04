//! What a package depends on and what depends on it, and pointing one of its
//! dependencies somewhere else.

use crate::action::use_action;
use crate::api::client;
use crate::routes::Route;
use crate::status::BuildStatusBadge;
use crate::widgets::{LinkRow, Modal, RowLink};
use aurcache_client::ExtendedPackage;
use dioxus::prelude::*;

/// Which side of a package's edges a list shows.
#[derive(Clone, PartialEq, Debug)]
enum Side {
    /// What the package depends on. Only these gate its build, and only these
    /// can be pointed elsewhere from here: an edge belongs to the package that
    /// declares it, so a dependent's dependency is changed from the
    /// dependent's own page.
    Dependencies {
        /// The package declaring them.
        of: String,
    },
    /// What depends on the package. One that is unsatisfied is waiting on
    /// *this* package, which is its problem to display, not a reason to flag
    /// anything here.
    Dependents,
}

#[component]
pub(super) fn Relations(pkg: ExtendedPackage, on_changed: EventHandler<()>) -> Element {
    let ExtendedPackage {
        name,
        dependencies,
        dependents,
        ..
    } = pkg;

    rsx! {
        RelationList {
            title: "Dependencies",
            empty: "Nothing — this package builds on its own.",
            items: dependencies,
            side: Side::Dependencies { of: name },
            on_changed,
        }
        RelationList {
            title: "Dependents",
            empty: "Nothing depends on this package.",
            items: dependents,
            side: Side::Dependents,
            on_changed,
        }
    }
}

#[component]
fn RelationList(
    title: String,
    empty: String,
    items: Vec<aurcache_client::PackageDependency>,
    side: Side,
    on_changed: EventHandler<()>,
) -> Element {
    let blocking = match side {
        Side::Dependencies { .. } => items.iter().filter(|item| !item.satisfied).count(),
        Side::Dependents => 0,
    };

    rsx! {
        div { class: "card bg-base-100 shadow-xl",
            div { class: "card-body",
                h2 { class: "card-title text-base",
                    "{title}"
                    if !items.is_empty() {
                        span { class: "badge badge-sm badge-neutral", "{items.len()}" }
                    }
                    if blocking > 0 {
                        span { class: "badge badge-sm badge-warning",
                            "{blocking} blocking"
                        }
                    }
                }
                if items.is_empty() {
                    p { class: "opacity-60 text-sm", "{empty}" }
                } else {
                    div { class: "overflow-x-auto",
                        table { class: "table table-zebra",
                            tbody {
                                for item in items.iter() {
                                    RelationRow {
                                        key: "{item.name}",
                                        item: item.clone(),
                                        side: side.clone(),
                                        on_changed,
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// One dependency, and why it is or is not holding the build back.
#[component]
fn RelationRow(
    item: aurcache_client::PackageDependency,
    side: Side,
    on_changed: EventHandler<()>,
) -> Element {
    let blocking = matches!(side, Side::Dependencies { .. }) && !item.satisfied;
    let mut replacing = use_signal(|| false);
    let to = Route::Package {
        pkgbase: item.name.clone(),
    };

    rsx! {
        LinkRow { to: to.clone(),
            td {
                // Monospace, matching the package page's heading: a pkgbase is
                // an identifier and reads as one.
                RowLink { class: "font-mono", to, "{item.name}" }
                if !item.version_constraint.is_empty() {
                    span { class: "font-mono text-xs opacity-60 ml-2", "{item.version_constraint}" }
                }
            }
            td {
                if blocking {
                    // Say what is actually wrong. "failed" alone does not
                    // distinguish a dependency that never built from one that built
                    // to a version too old to satisfy the constraint — and the
                    // second looks healthy everywhere else.
                    match item.built_version.clone() {
                        Some(built) => rsx! {
                            span { class: "font-mono text-xs opacity-70", "has {built}" }
                            span { class: "badge badge-warning badge-sm ml-2", "too old" }
                        },
                        None => rsx! {
                            span { class: "badge badge-warning badge-sm", "never built" }
                        },
                    }
                } else if let Some(built) = item.built_version.clone() {
                    span { class: "font-mono text-xs opacity-50", "{built}" }
                }
            }
            td { BuildStatusBadge { status: item.status } }
            if let Side::Dependencies { .. } = side {
                td { class: "text-right",
                    button {
                        class: "btn btn-ghost btn-xs",
                        // A button inside a clickable row has to claim its own
                        // click, or pressing it also navigates away.
                        onclick: move |e: MouseEvent| {
                            e.stop_propagation();
                            replacing.set(true);
                        },
                        "Replace"
                    }
                }
            }
        }
        // Beside the row rather than inside it: a modal is positioned against
        // the viewport regardless of where it is mounted, and a div inside a
        // tr would be invalid table markup.
        if let Side::Dependencies { of } = side {
            if replacing() {
                ReplaceDependencyDialog {
                    dependent: of,
                    dependency: item.name,
                    on_close: move |()| replacing.set(false),
                    on_changed,
                }
            }
        }
    }
}

/// Point one dependency at something else, or drop it.
///
/// Which packages could stand in is a question only the server can answer --
/// it weighs the official repositories, everything tracked here and an AUR
/// `provides` search, in that order -- so this is a thin view over one
/// endpoint rather than a picker filtering a list the page already had. It is
/// mounted only while open, which is what makes that request happen on opening
/// and not on every page load.
#[component]
fn ReplaceDependencyDialog(
    dependent: String,
    dependency: String,
    on_close: EventHandler<()>,
    on_changed: EventHandler<()>,
) -> Element {
    let options = use_resource({
        let dependent = dependent.clone();
        let dependency = dependency.clone();
        move || {
            let dependent = dependent.clone();
            let dependency = dependency.clone();
            async move {
                client()?
                    .dependency_options(&dependent, &dependency)
                    .await
                    .map_err(|e| e.to_string())
            }
        }
    });

    let action = use_action();
    let busy = action.busy();
    // A callback rather than a closure, so it is `Copy` and can be handed to
    // the drop button and to every candidate row.
    let apply = use_callback({
        let dependent = dependent.clone();
        let dependency = dependency.clone();
        move |replacement: Option<String>| {
            let dependent = dependent.clone();
            let dependency = dependency.clone();
            spawn(async move {
                let replaced = action
                    .run(async {
                        client()?
                            .replace_dependency(&dependent, &dependency, replacement.as_deref())
                            .await
                            .map_err(|e| e.to_string())
                    })
                    .await;
                if replaced.is_some() {
                    on_changed.call(());
                    on_close.call(());
                }
            });
        }
    });

    rsx! {
        Modal {
            open: true,
            label: "Replace dependency",
            on_close,
            locked: busy,
            class: "max-w-2xl",
            h3 { class: "font-bold text-lg", "Replace {dependency}" }
            p { class: "text-sm opacity-70 pt-1",
                "Changes what {dependent} is built against. "
                "The choice sticks: resolution prefers the dependency a package already has."
            }

            if let Some(message) = action.error() {
                div { class: "alert alert-error text-sm mt-3", span { "{message}" } }
            }

            match &*options.read_unchecked() {
                None => rsx! {
                    div { class: "flex justify-center p-8",
                        span { class: "loading loading-spinner" }
                    }
                },
                Some(Err(e)) => rsx! {
                    div { class: "alert alert-error text-sm mt-3", span { "{e}" } }
                },
                Some(Ok(options)) => {
                    let dropped = !options.official.is_empty()
                        && options.official.len() == options.declared_names.len();
                    rsx! {
                        div { class: "text-xs opacity-60 pt-3 font-mono",
                            if options.declared_names.is_empty() {
                                "no longer declared by {dependent}"
                            } else {
                                "declared as {options.declared_names.join(\", \")}"
                            }
                            if !options.version_constraint.is_empty() {
                                " {options.version_constraint}"
                            }
                        }

                        // Rare, and worth its own action rather than a row
                        // in the list: nothing is chosen, the edge simply
                        // stops existing because pacman can satisfy it.
                        if dropped {
                            div { class: "alert alert-info text-sm mt-3 flex-wrap",
                                span {
                                    "The official repositories now publish this. "
                                    "AURCache does not have to build anything for it."
                                }
                                button {
                                    class: "btn btn-sm",
                                    disabled: busy,
                                    onclick: move |_| apply(None),
                                    "Drop the dependency"
                                }
                            }
                        }

                        if let Some(message) = options.aur_error.clone() {
                            div { class: "alert alert-warning text-sm mt-3",
                                span {
                                    "The AUR could not be searched ({message}), so only "
                                    "packages already tracked here are listed."
                                }
                            }
                        }

                        if options.candidates.is_empty() {
                            p { class: "opacity-60 text-sm pt-4",
                                if options.aur_error.is_some() {
                                    "Nothing tracked here provides it."
                                } else {
                                    "Nothing else provides it, here or in the AUR."
                                }
                            }
                        } else {
                            ul { class: "divide-y divide-base-300 pt-2 max-h-80 overflow-y-auto",
                                for candidate in options.candidates.iter() {
                                    CandidateRow {
                                        key: "{candidate.pkgbase}",
                                        candidate: candidate.clone(),
                                        busy,
                                        on_pick: move |pkgbase: String| apply(Some(pkgbase)),
                                    }
                                }
                            }
                        }
                    }
                }
            }

            div { class: "modal-action",
                button {
                    class: "btn btn-sm",
                    disabled: busy,
                    onclick: move |_| on_close.call(()),
                    "Cancel"
                }
            }
        }
    }
}

/// One package that could take the edge over.
#[component]
fn CandidateRow(
    candidate: aurcache_client::DependencyCandidate,
    busy: bool,
    on_pick: EventHandler<String>,
) -> Element {
    let pkgbase = candidate.pkgbase.clone();

    rsx! {
        li { class: "py-2 flex items-center gap-2 flex-wrap",
            span { class: "font-mono text-sm break-all", "{candidate.pkgbase}" }
            match candidate.source {
                aurcache_client::CandidateSource::Tracked => rsx! {
                    span { class: "badge badge-sm badge-neutral", "tracked" }
                },
                // Says what picking it costs: a package that is not here yet
                // gets added and built before the dependent can use it.
                aurcache_client::CandidateSource::Aur => rsx! {
                    span { class: "badge badge-sm badge-outline", "AUR — would be added" }
                },
            }
            if let Some(version) = candidate.version.clone() {
                span { class: "font-mono text-xs opacity-60", "{version}" }
            }
            match candidate.verdict {
                aurcache_client::ReplacementVerdict::Satisfied => rsx! {},
                aurcache_client::ReplacementVerdict::Unknown => rsx! {
                    span { class: "badge badge-sm badge-ghost", "not built yet" }
                },
                aurcache_client::ReplacementVerdict::Unsatisfied => rsx! {
                    span { class: "badge badge-sm badge-warning", "too old" }
                },
            }
            div { class: "flex-1" }
            button {
                class: "btn btn-sm btn-primary",
                disabled: busy,
                onclick: move |_| on_pick.call(pkgbase.clone()),
                "Use"
            }
        }
    }
}
