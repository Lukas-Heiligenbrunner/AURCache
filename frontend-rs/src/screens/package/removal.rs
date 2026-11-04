//! Removing a package: plainly when nothing needs it, or by first pointing its
//! dependents somewhere else when it is only here as a dependency.

use crate::action::use_action;
use crate::api::client;
use crate::listing::ViewParams;
use crate::routes::Route;
use crate::widgets::{ConfirmDialog, Consequence, Modal};
use aurcache_client::DependencyOptions;
use dioxus::prelude::*;
use std::collections::{HashMap, HashSet};

/// One replacement, and the dependents it could take over from.
#[derive(Clone, Debug, PartialEq)]
struct SharedCandidate {
    pkgbase: String,
    source: aurcache_client::CandidateSource,
    /// The dependents whose options list it, in the order they were given.
    serves: Vec<String>,
}

/// Merge each dependent's candidates into one list, best first.
///
/// Removing a package means every dependent has to stop needing it, so the
/// candidate that matters is the one that can take over from the most of them
/// -- a replacement serving four of five dependents leaves one package to deal
/// with, and one serving all five leaves none.
///
/// Among candidates serving equally many, the best position any dependent's
/// list gave it wins, so the server's ranking carries through instead of being
/// flattened into alphabetical order.
fn shared_candidates(per_dependent: &[DependentOptions]) -> Vec<SharedCandidate> {
    /// A candidate with the best position any dependent's list gave it.
    struct Ranked {
        candidate: SharedCandidate,
        best_rank: usize,
    }

    // Indexed by pkgbase, so the merge stays linear in dependents × candidates.
    let mut index: HashMap<&str, usize> = HashMap::new();
    let mut merged: Vec<Ranked> = Vec::new();
    for DependentOptions { dependent, options } in per_dependent {
        for (rank, candidate) in options.candidates.iter().enumerate() {
            match index.get(candidate.pkgbase.as_str()) {
                Some(&at) => {
                    let ranked = &mut merged[at];
                    ranked.candidate.serves.push(dependent.clone());
                    ranked.best_rank = ranked.best_rank.min(rank);
                }
                None => {
                    index.insert(candidate.pkgbase.as_str(), merged.len());
                    merged.push(Ranked {
                        candidate: SharedCandidate {
                            pkgbase: candidate.pkgbase.clone(),
                            source: candidate.source,
                            serves: vec![dependent.clone()],
                        },
                        best_rank: rank,
                    });
                }
            }
        }
    }

    merged.sort_by(|a, b| {
        b.candidate
            .serves
            .len()
            .cmp(&a.candidate.serves.len())
            .then_with(|| a.best_rank.cmp(&b.best_rank))
            .then_with(|| a.candidate.pkgbase.cmp(&b.candidate.pkgbase))
    });
    merged.into_iter().map(|ranked| ranked.candidate).collect()
}

/// What the server offers one dependent in place of the package going away.
#[derive(Clone, Debug, PartialEq)]
struct DependentOptions {
    dependent: String,
    options: DependencyOptions,
}

/// What one dependent ends up depending on.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Choice {
    /// Drop the edge. The official repositories publish every name this
    /// dependent declares, so nothing has to be built for it at all.
    Official,
    /// Depend on this package base instead.
    Package(String),
}

/// One thing that can be offered as a replacement, and who it can serve.
#[derive(Clone, Debug, PartialEq)]
struct Offer {
    choice: Choice,
    /// `None` for the official repositories, which are not a package.
    source: Option<aurcache_client::CandidateSource>,
    serves: Vec<String>,
}

/// Everything that could take over from this package, best first.
///
/// The official repositories lead where they can take a dependent at all, even
/// if that is only one of several. Coverage does not rank them against the
/// packages below: dropping an edge builds nothing, so for a dependent they
/// can serve there is no better answer, and a package serving four others does
/// not make it a worse one for the fifth.
///
/// They are judged per dependent, not for the removal as a whole. One package
/// can declare `git`, which `extra` publishes, while another declares this
/// package's own name, which it does not -- the first is dropped and the
/// second still needs somewhere to go.
fn offers_for(loaded: &[DependentOptions]) -> Vec<Offer> {
    let mut offers = Vec::new();

    let official: Vec<String> = loaded
        .iter()
        .filter(|loaded| {
            // Every declared name, not merely one of them: a dependent whose
            // other name is unpublished would be left with a dependency
            // nothing satisfies.
            let options = &loaded.options;
            !options.official.is_empty() && options.official.len() == options.declared_names.len()
        })
        .map(|loaded| loaded.dependent.clone())
        .collect();
    if !official.is_empty() {
        offers.push(Offer {
            choice: Choice::Official,
            source: None,
            serves: official,
        });
    }

    offers.extend(
        shared_candidates(loaded)
            .into_iter()
            .map(|candidate| Offer {
                choice: Choice::Package(candidate.pkgbase),
                source: Some(candidate.source),
                serves: candidate.serves,
            }),
    );

    offers
}

/// Work out what each dependent ends up with, given what is selected.
///
/// More than one replacement can be picked, because one need not serve
/// everybody -- `git-git` may cover the dependents asking for `git` while the
/// one asking for `git-git` itself needs something else entirely. Each
/// dependent takes the first selected offer that can serve it, in the order
/// the offers are listed, so the answer is the one the list already put
/// highest and selecting a further option never changes what an earlier one
/// was doing.
///
/// `None` for a dependent means nothing selected can serve it: it has to be
/// removed, or the package it needs stays.
fn assign(offers: &[Offer], selected: &[Choice], dependents: &[String]) -> Vec<Assignment> {
    dependents
        .iter()
        .map(|dependent| Assignment {
            dependent: dependent.clone(),
            choice: offers
                .iter()
                .find(|offer| selected.contains(&offer.choice) && offer.serves.contains(dependent))
                .map(|offer| offer.choice.clone()),
        })
        .collect()
}

/// One dependent, and the selected offer that will serve it; see [`assign`].
#[derive(Clone, Debug, PartialEq, Eq)]
struct Assignment {
    dependent: String,
    /// `None` when nothing selected can serve it.
    choice: Option<Choice>,
}

/// One dependent pointed at what will serve it, once the removal is applied.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Replacement {
    dependent: String,
    choice: Choice,
}

/// What applying a removal does, given what is selected and ticked.
#[derive(Clone, Debug, PartialEq, Eq)]
struct RemovalPlan {
    /// Every dependent, and the selected offer serving it, if any.
    assignments: Vec<Assignment>,
    /// Dependents neither pointed elsewhere nor removed. Each still needs the
    /// package, and while any does, removing it does nothing.
    unaccounted: usize,
    /// The edits to make, one per dependent that survives.
    replacements: Vec<Replacement>,
    /// Every package that goes, in the order the walk found them: the ticked
    /// dependents and whatever cannot survive without them.
    doomed: Vec<String>,
}

/// Work out what a removal does.
///
/// `cascade` is what removing the ticked dependents takes with it, once that
/// is known; until then only the ticked ones themselves are counted.
fn removal_plan(
    offers: &[Offer],
    dependents: &[String],
    selected: &[Choice],
    removing: &[String],
    cascade: Option<&[String]>,
) -> RemovalPlan {
    let assignments = assign(offers, selected, dependents);
    let survives = |assignment: &&Assignment| !removing.contains(&assignment.dependent);
    RemovalPlan {
        unaccounted: assignments
            .iter()
            .filter(survives)
            .filter(|assignment| assignment.choice.is_none())
            .count(),
        replacements: assignments
            .iter()
            .filter(survives)
            .filter_map(|assignment| {
                Some(Replacement {
                    dependent: assignment.dependent.clone(),
                    choice: assignment.choice.clone()?,
                })
            })
            .collect(),
        doomed: cascade.unwrap_or(removing).to_vec(),
        assignments,
    }
}

/// Put `item` in `list` or take it out, keeping the order the rest were added
/// in.
fn set_member<T: PartialEq>(list: &mut Vec<T>, item: T, member: bool) {
    list.retain(|existing| existing != &item);
    if member {
        list.push(item);
    }
}

/// Remove a package that is only here as a dependency.
///
/// Plain removal cannot: it clears the direct-request flag, which is already
/// clear, and then keeps the package because its dependents still reach it --
/// a button that did nothing. What has to happen is that every dependent stops
/// needing it, and once that is true the collection that runs after each edit
/// takes the package by itself. So this is a shortcut for editing each
/// dependent's dependency in turn, not an operation of its own, and it goes
/// through the same endpoint one Replace button does.
#[component]
pub(super) fn ReplaceAndRemoveCard(
    pkgbase: String,
    dependents: Vec<aurcache_client::PackageDependency>,
    on_changed: EventHandler<()>,
) -> Element {
    let mut open = use_signal(|| false);
    let count = dependents.len();

    rsx! {
        div { class: "card bg-base-100 shadow-xl border border-error/30",
            div { class: "card-body",
                h2 { class: "card-title text-base text-error", "Replace & remove" }
                p { class: "text-xs opacity-60 max-w-prose",
                    "This package is only here because "
                    if count == 1 { "one package needs" } else { "{count} packages need" }
                    " it. Removing it means pointing "
                    if count == 1 { "that package" } else { "those packages" }
                    " at something else first; once nothing needs it, it and its "
                    "builds are deleted."
                }
                div {
                    button {
                        class: "btn btn-error btn-sm btn-outline",
                        onclick: move |_| open.set(true),
                        "Replace & remove"
                    }
                }
            }
        }
        if open() {
            ReplaceAndRemoveDialog {
                pkgbase,
                dependents,
                on_close: move |()| open.set(false),
                on_changed,
            }
        }
    }
}

/// At most this many package fetches in flight per breadth level: the walk
/// fans out over the dependent graph, and one `join_all` per level fires a
/// request per dependent at once.
const CASCADE_FETCH_CONCURRENCY: usize = 8;

/// Every package that would go with `roots`, the roots included.
///
/// Removing a package is not a local act: everything that needs it stops
/// existing too, and everything that needs those. Walking the dependents
/// transitively is the only way to say what a removal costs before it happens
/// -- and without it a cascade that stops one level down leaves the package it
/// was meant to free still needed, so nothing is removed at all.
async fn cascade_closure(
    client: &aurcache_client::AurCacheClient,
    roots: &[String],
) -> Result<Vec<String>, String> {
    // `seen` answers membership; `order` keeps the breadth-first order. A
    // name is marked seen when it is queued, so every frontier is already free
    // of duplicates and of anything an earlier level visited.
    let mut seen: HashSet<String> = HashSet::new();
    let mut order: Vec<String> = Vec::new();
    let mut frontier: Vec<String> = roots
        .iter()
        .filter(|root| seen.insert((*root).clone()))
        .cloned()
        .collect();

    while !frontier.is_empty() {
        let mut fetched = Vec::with_capacity(frontier.len());
        for chunk in frontier.chunks(CASCADE_FETCH_CONCURRENCY) {
            fetched.extend(
                futures_util::future::join_all(chunk.iter().map(|name| client.get_package(name)))
                    .await,
            );
        }

        let mut next = Vec::new();
        for (name, result) in frontier.iter().zip(fetched) {
            order.push(name.clone());
            let package = result.map_err(|e| format!("{name}: {e}"))?;
            for dependent in package.dependents {
                if seen.insert(dependent.name.clone()) {
                    next.push(dependent.name);
                }
            }
        }
        frontier = next;
    }

    Ok(order)
}

/// Point every dependent somewhere else, then let the package fall away.
#[component]
fn ReplaceAndRemoveDialog(
    pkgbase: String,
    dependents: Vec<aurcache_client::PackageDependency>,
    on_close: EventHandler<()>,
    on_changed: EventHandler<()>,
) -> Element {
    // One request per dependent, all in flight together. They are asked
    // separately because they are answered separately -- two packages needing
    // this one can declare different names for it and hold different
    // constraints -- but nothing about them is sequential.
    let options = use_resource({
        let pkgbase = pkgbase.clone();
        let dependents: Vec<String> = dependents.iter().map(|d| d.name.clone()).collect();
        move || {
            let pkgbase = pkgbase.clone();
            let dependents = dependents.clone();
            async move {
                let client = client()?;
                let answers = futures_util::future::join_all(
                    dependents
                        .iter()
                        .map(|dependent| client.dependency_options(dependent, &pkgbase)),
                )
                .await;

                dependents
                    .into_iter()
                    .zip(answers)
                    .map(|(dependent, answer)| match answer {
                        Ok(options) => Ok(DependentOptions { dependent, options }),
                        Err(e) => Err(format!("{dependent}: {e}")),
                    })
                    .collect::<Result<Vec<_>, String>>()
            }
        }
    });

    let mut selected = use_signal(Vec::<Choice>::new);
    let mut removing = use_signal(Vec::<String>::new);
    let mut busy = use_signal(|| false);
    // What is happening right now, step by step. Applying is one request per
    // dependent and one per package being removed, each of which reads a
    // source or sweeps the graph on the server -- long enough that a bare
    // spinner leaves someone wondering whether it is working or hung.
    let mut progress = use_signal(|| Option::<String>::None);
    let mut errors = use_signal(Vec::<String>::new);

    // What removing the ticked dependents actually takes with it. Reruns as
    // they are ticked, because the answer is a walk of the dependent graph
    // rather than anything this page already knows.
    let cascade = use_resource(move || {
        let roots = removing();
        async move {
            if roots.is_empty() {
                return Ok(Vec::new());
            }
            let client = client()?;
            cascade_closure(&client, &roots).await
        }
    });

    // A callback, so the button below can hand it a plan without the closure
    // holding a copy of the package's name.
    let apply = use_callback({
        let pkgbase = pkgbase.clone();
        move |plan: RemovalPlan| {
            let pkgbase = pkgbase.clone();
            spawn(async move {
                busy.set(true);
                errors.set(Vec::new());
                let mut failed = Vec::new();
                let steps = plan.replacements.len() + plan.doomed.len();
                let mut step = 0;

                let client = match client() {
                    Ok(client) => client,
                    Err(e) => {
                        busy.set(false);
                        errors.set(vec![e]);
                        return;
                    }
                };

                // In sequence: each one runs a collection on the way out, and
                // two of those racing would each be deciding what is still
                // reachable while the other changed it.
                for Replacement { dependent, choice } in &plan.replacements {
                    step += 1;
                    progress.set(Some(format!(
                        "{step}/{steps} — pointing {dependent} elsewhere"
                    )));
                    let replacement = match choice {
                        Choice::Official => None,
                        Choice::Package(pkgbase) => Some(pkgbase.as_str()),
                    };
                    if let Err(e) = client
                        .replace_dependency(dependent, &pkgbase, replacement)
                        .await
                    {
                        failed.push(format!("{dependent}: {e}"));
                    }
                }
                // Furthest-out first: a package still has dependents until the
                // things above it are gone, and only then does clearing it free
                // what is underneath.
                for dependent in plan.doomed.iter().rev() {
                    step += 1;
                    progress.set(Some(format!("{step}/{steps} — removing {dependent}")));
                    if let Err(e) = client.delete_package(dependent).await {
                        failed.push(format!("{dependent}: {e}"));
                    }
                }

                busy.set(false);
                progress.set(None);
                if failed.is_empty() {
                    // Nothing needs it any more, so the collection after the
                    // last edit took it. Its page would now be a 404.
                    navigator().push(Route::Packages {
                        view: ViewParams::default(),
                        q: String::new(),
                    });
                } else {
                    errors.set(failed);
                    on_changed.call(());
                }
            });
        }
    });

    rsx! {
        Modal {
            open: true,
            label: "Replace and remove",
            on_close,
            locked: busy(),
            class: "max-w-4xl",
            h3 { class: "font-bold text-lg", "Remove {pkgbase}" }

            for message in errors() {
                div { class: "alert alert-error text-sm mt-3", span { "{message}" } }
            }

            match &*options.read_unchecked() {
                None => rsx! {
                    div { class: "flex flex-col items-center gap-2 p-8",
                        span { class: "loading loading-spinner" }
                        span { class: "text-xs opacity-60", "Looking for replacements…" }
                    }
                },
                Some(Err(e)) => rsx! {
                    div { class: "alert alert-error text-sm mt-3", span { "{e}" } }
                },
                Some(Ok(loaded)) => {
                    let offers = offers_for(loaded);
                    let dependents: Vec<String> =
                        loaded.iter().map(|loaded| loaded.dependent.clone()).collect();
                    let total = dependents.len();
                    let plan = removal_plan(
                        &offers,
                        &dependents,
                        &selected(),
                        &removing(),
                        match &*cascade.read_unchecked() {
                            Some(Ok(closure)) => Some(closure.as_slice()),
                            _ => None,
                        },
                    );
                    // Every dependent has to be accounted for. One left neither
                    // pointed elsewhere nor removed still needs this package,
                    // and then removing it does nothing -- which is the button
                    // this card exists to replace.
                    let ready = plan.unaccounted == 0;
                    let unaccounted = plan.unaccounted;
                    let assignments = plan.assignments.clone();

                    rsx! {
                        p { class: "text-sm opacity-70 pt-1",
                            "Needed by {total} "
                            if total == 1 { "package" } else { "packages" }
                            ". Choose what can stand in for it; anything left over "
                            "has to go with it."
                        }

                        div { class: "grid grid-cols-1 md:grid-cols-2 gap-4 pt-3",
                            div {
                                h4 { class: "font-semibold text-sm pb-1", "Replace with" }
                                if offers.is_empty() {
                                    p { class: "opacity-60 text-sm",
                                        "Nothing else provides what these packages need."
                                    }
                                } else {
                                    ul { class: "divide-y divide-base-300 max-h-72 overflow-y-auto",
                                        for offer in offers.iter() {
                                            OfferRow {
                                                key: "{offer_key(&offer.choice)}",
                                                offer: offer.clone(),
                                                total,
                                                busy: busy(),
                                                picked: selected().contains(&offer.choice),
                                                on_toggle: {
                                                    let choice = offer.choice.clone();
                                                    move |on: bool| {
                                                        selected.with_mut(|selected| {
                                                            set_member(selected, choice.clone(), on);
                                                        });
                                                    }
                                                },
                                            }
                                        }
                                    }
                                }
                            }

                            div {
                                h4 { class: "font-semibold text-sm pb-1", "Dependents" }
                                ul { class: "divide-y divide-base-300 max-h-72 overflow-y-auto",
                                    for Assignment { dependent, choice } in assignments.iter() {
                                        DependentRow {
                                            key: "{dependent}",
                                            dependent: dependent.clone(),
                                            choice: choice.clone(),
                                            busy: busy(),
                                            marked: removing().contains(dependent),
                                            on_remove: {
                                                let dependent = dependent.clone();
                                                move |on: bool| {
                                                    removing.with_mut(|removing| {
                                                        set_member(removing, dependent.clone(), on);
                                                    });
                                                }
                                            },
                                        }
                                    }
                                }
                            }
                        }

                        // What the ticked removals really cost. A dependent has
                        // dependents of its own, and they cannot be left
                        // needing something that is going away.
                        if !removing().is_empty() {
                            match &*cascade.read_unchecked() {
                                None => rsx! {
                                    div { class: "text-xs opacity-60 pt-3",
                                        "Working out what else that removes…"
                                    }
                                },
                                Some(Err(e)) => rsx! {
                                    div { class: "alert alert-error text-sm mt-3", span { "{e}" } }
                                },
                                Some(Ok(closure)) => {
                                    let extra: Vec<String> = closure
                                        .iter()
                                        .filter(|name| !removing().contains(name))
                                        .cloned()
                                        .collect();
                                    rsx! {
                                        div { class: "alert alert-warning text-sm mt-3",
                                            if extra.is_empty() {
                                                span { "Removes {removing().len()} package(s)." }
                                            } else {
                                                span {
                                                    "Also removes {extra.join(\", \")}, which "
                                                    "cannot survive without them."
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }

                        div { class: "modal-action",
                            if let Some(step) = progress() {
                                span { class: "text-xs opacity-70 self-center font-mono",
                                    "{step}"
                                }
                            } else if !ready {
                                span { class: "text-xs opacity-60 self-center",
                                    "{unaccounted} dependent(s) still need {pkgbase}"
                                }
                            }
                            button {
                                class: "btn btn-sm",
                                disabled: busy(),
                                onclick: move |_| on_close.call(()),
                                "Cancel"
                            }
                            button {
                                class: "btn btn-error btn-sm",
                                disabled: busy() || !ready,
                                onclick: move |_| apply(plan.clone()),
                                if busy() {
                                    span { class: "loading loading-spinner loading-xs" }
                                }
                                "Replace & remove"
                            }
                        }
                    }
                }
            }
        }
    }
}

/// A stable identity for a choice, for keying its row.
fn offer_key(choice: &Choice) -> String {
    match choice {
        Choice::Official => "::official".to_string(),
        Choice::Package(pkgbase) => pkgbase.clone(),
    }
}

/// One thing that could stand in, and how much of the job it does.
#[component]
fn OfferRow(
    offer: Offer,
    total: usize,
    picked: bool,
    busy: bool,
    on_toggle: EventHandler<bool>,
) -> Element {
    rsx! {
        li { class: "py-2",
            label { class: "label cursor-pointer justify-start gap-3",
                input {
                    r#type: "checkbox",
                    class: "checkbox checkbox-sm",
                    checked: picked,
                    disabled: busy,
                    onchange: move |event| on_toggle.call(event.checked()),
                }
                match &offer.choice {
                    Choice::Official => rsx! {
                        span { class: "text-sm", "Official repositories" }
                        span { class: "badge badge-sm badge-info", "nothing to build" }
                    },
                    Choice::Package(pkgbase) => rsx! {
                        span { class: "font-mono text-sm break-all", "{pkgbase}" }
                        match offer.source {
                            Some(aurcache_client::CandidateSource::Aur) => rsx! {
                                span { class: "badge badge-sm badge-outline", "AUR" }
                            },
                            _ => rsx! {
                                span { class: "badge badge-sm badge-neutral", "tracked" }
                            },
                        }
                    },
                }
                if offer.serves.len() == total {
                    span { class: "badge badge-sm badge-success", "covers all {total}" }
                } else {
                    span { class: "badge badge-sm badge-ghost",
                        "covers {offer.serves.len()} of {total}"
                    }
                }
            }
        }
    }
}

/// One dependent, and what it ends up with.
#[component]
fn DependentRow(
    dependent: String,
    choice: Option<Choice>,
    marked: bool,
    busy: bool,
    on_remove: EventHandler<bool>,
) -> Element {
    rsx! {
        li { class: "py-2",
            match choice {
                // Ticked by what is selected on the left rather than by hand:
                // a dependent does not get a say in which replacement covers
                // it, only in whether it survives at all.
                Some(choice) => rsx! {
                    div { class: "flex items-center gap-3 px-1",
                        input {
                            r#type: "checkbox",
                            class: "checkbox checkbox-sm checkbox-success",
                            checked: true,
                            disabled: true,
                        }
                        span { class: "font-mono text-sm break-all", "{dependent}" }
                        span { class: "text-xs opacity-60",
                            match &choice {
                                Choice::Official => "→ official repositories".to_string(),
                                Choice::Package(pkgbase) => format!("→ {pkgbase}"),
                            }
                        }
                    }
                },
                None => rsx! {
                    label { class: "label cursor-pointer justify-start gap-3",
                        input {
                            r#type: "checkbox",
                            class: "checkbox checkbox-sm checkbox-warning",
                            checked: marked,
                            disabled: busy,
                            onchange: move |event| on_remove.call(event.checked()),
                        }
                        span { class: "font-mono text-sm break-all", "{dependent}" }
                        span { class: "text-xs opacity-60", "remove" }
                    }
                },
            }
        }
    }
}

/// "Remove" rather than "delete" because that is what the server does when
/// something still needs the package: it clears the direct-request flag and the
/// card then becomes this package's replacement-and-removal.
///
/// How deep the remove goes is decided here, not by the server: the page
/// already knows whether anything depends on the package.
///
/// With no dependents the delete is total — the package, its builds and the
/// dependencies only it used are all dropped, so the page leaves for the list.
/// With dependents the package cannot go: removing only unflags it, and the
/// refresh this card triggers makes it the replacement-and-removal card.
#[component]
pub(super) fn RemoveCard(
    pkgbase: String,
    dependents: usize,
    on_changed: EventHandler<()>,
) -> Element {
    let mut confirming = use_signal(|| false);
    let action = use_action();

    let remove = move |pkgbase: String| async move {
        let removed = action
            .run(async {
                client()?
                    .delete_package(&pkgbase)
                    .await
                    .map_err(|e| e.to_string())
            })
            .await;
        if removed.is_none() {
            return;
        }
        confirming.set(false);
        if dependents > 0 {
            // The package stayed on as a dependency, so going to the list
            // would say nothing about it. Refresh in place: the card becomes
            // the replacement one.
            on_changed.call(());
        } else {
            // The package may no longer exist, so going back to it would land
            // on an error page.
            navigator().push(Route::Packages {
                view: ViewParams::default(),
                q: String::new(),
            });
        }
    };

    rsx! {
        div { class: "card bg-base-100 shadow-xl border border-error/30",
            div { class: "card-body",
                h2 { class: "card-title text-base text-error", "Remove" }
                p { class: "text-xs opacity-60 max-w-prose",
                    if dependents > 0 {
                        "This package has dependents, so removing only marks it as no longer explicitly required. \
                         It stays as a dependency, and this card then becomes its replacement and removal."
                    } else {
                        "Nothing depends on it, so removing deletes the package — its builds go too, \
                         and any dependency that was only installed for it."
                    }
                }
                if let Some(message) = action.error() {
                    div { class: "alert alert-error text-sm", span { "{message}" } }
                }
                div {
                    button {
                        class: "btn btn-error btn-sm btn-outline",
                        onclick: move |_| confirming.set(true),
                        "Remove package"
                    }
                }
            }
        }

        ConfirmDialog {
            open: confirming(),
            label: "Confirm removal",
            title: "Remove {pkgbase}?",
            cancel: "Cancel",
            confirm: "Remove",
            consequence: Consequence::Destructive,
            busy: action.busy(),
            on_cancel: move |()| confirming.set(false),
            on_confirm: move |()| {
                spawn(remove(pkgbase.clone()));
            },
            if dependents > 0 {
                "It stops being a requested package and stays as a dependency for the packages that \
                 need it. Nothing is deleted; replacing and removing it is still open to you from this card."
            } else {
                "Nothing depends on it. It and its build history are deleted, and so is anything that \
                 was only here as its dependency. This cannot be undone."
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aurcache_client::{CandidateSource, DependencyCandidate, ReplacementVerdict};

    fn candidate(pkgbase: &str) -> DependencyCandidate {
        DependencyCandidate {
            pkgbase: pkgbase.to_string(),
            source: CandidateSource::Tracked,
            version: None,
            verdict: ReplacementVerdict::Satisfied,
        }
    }

    fn options(declared: &[&str], official: &[&str]) -> DependencyOptions {
        DependencyOptions {
            dependent: "dependent".to_string(),
            current: "current".to_string(),
            declared_names: declared.iter().copied().map(String::from).collect(),
            version_constraint: String::new(),
            official: official.iter().copied().map(String::from).collect(),
            candidates: vec![],
            aur_error: None,
        }
    }

    /// What the server offered `dependent`.
    fn offered(dependent: &str, options: DependencyOptions) -> DependentOptions {
        DependentOptions {
            dependent: dependent.to_string(),
            options,
        }
    }

    /// A dependent offered these candidates and nothing else.
    fn wanting(dependent: &str, candidates: Vec<DependencyCandidate>) -> DependentOptions {
        offered(
            dependent,
            DependencyOptions {
                candidates,
                ..options(&[], &[])
            },
        )
    }

    /// Removing the package needs every dependent to stop wanting it, so the
    /// candidate that serves the most of them leads -- even where another one
    /// was ranked higher by the dependent that offered it.
    #[test]
    fn the_candidate_serving_the_most_dependents_leads() {
        let merged = shared_candidates(&[
            wanting("one", vec![candidate("narrow"), candidate("wide")]),
            wanting("two", vec![candidate("wide")]),
        ]);

        assert_eq!(merged[0].pkgbase, "wide");
        assert_eq!(merged[0].serves, vec!["one", "two"]);
        assert_eq!(merged[1].pkgbase, "narrow");
        assert_eq!(merged[1].serves, vec!["one"]);
    }

    /// Among candidates that serve equally many, the server's own ranking
    /// carries through instead of collapsing into alphabetical order.
    #[test]
    fn equal_coverage_keeps_the_servers_order() {
        let merged =
            shared_candidates(&[wanting("one", vec![candidate("zebra"), candidate("alpha")])]);

        assert_eq!(
            merged
                .iter()
                .map(|c| c.pkgbase.as_str())
                .collect::<Vec<_>>(),
            vec!["zebra", "alpha"]
        );
    }

    /// A candidate ranked poorly by one dependent and well by another is not
    /// penalised for the worse showing: it is the same package either way.
    #[test]
    fn the_best_position_any_dependent_gave_it_counts() {
        let merged = shared_candidates(&[
            wanting("one", vec![candidate("first"), candidate("second")]),
            wanting("two", vec![candidate("second"), candidate("first")]),
        ]);

        assert_eq!(merged.len(), 2);
        assert!(merged.iter().all(|c| c.serves.len() == 2));
    }

    #[test]
    fn nothing_to_merge_yields_nothing() {
        assert!(shared_candidates(&[]).is_empty());
    }

    /// The repositories are offered for a dependent they fully cover, and only
    /// that one. A package declaring this package's own name is not served by
    /// them however many of its neighbours are -- which is the difference
    /// between `git` moving into `extra` and something that really does need
    /// `git-git`.
    #[test]
    fn the_repositories_are_offered_per_dependent() {
        let offers = offers_for(&[
            offered("wants-git", options(&["git"], &["git"])),
            offered("wants-git-git", options(&["git-git"], &[])),
        ]);

        assert_eq!(offers[0].choice, Choice::Official);
        assert_eq!(
            offers[0].serves,
            vec!["wants-git"],
            "only the dependent they cover"
        );
    }

    /// A dependent they cover only half of is not covered: the other name
    /// would be left with nothing satisfying it.
    #[test]
    fn a_partly_published_dependent_is_not_offered_the_repositories() {
        let offers = offers_for(&[offered(
            "one",
            options(&["libfoo", "libfoo-compat"], &["libfoo"]),
        )]);

        assert!(offers.iter().all(|offer| offer.choice != Choice::Official));
    }

    /// The repositories lead where they can take a dependent at all. Dropping
    /// an edge builds nothing, so for a dependent they serve there is no
    /// better answer -- a package covering four others is not a better one for
    /// the fifth.
    #[test]
    fn the_repositories_lead_even_when_they_cover_less() {
        let mut wide = options(&["libfoo"], &[]);
        wide.candidates = vec![candidate("wide")];
        let mut published = options(&["libfoo"], &["libfoo"]);
        published.candidates = vec![candidate("wide")];

        let offers = offers_for(&[
            offered("one", published),
            offered("two", wide.clone()),
            offered("three", wide),
        ]);

        assert_eq!(offers[0].choice, Choice::Official);
        assert_eq!(offers[0].serves.len(), 1);
        assert_eq!(offers[1].choice, Choice::Package("wide".to_string()));
        assert_eq!(offers[1].serves.len(), 3);
    }

    fn offer(choice: Choice, serves: &[&str]) -> Offer {
        Offer {
            choice,
            source: None,
            serves: serves.iter().copied().map(String::from).collect(),
        }
    }

    /// More than one replacement can be picked, because one need not serve
    /// everybody. Each dependent then takes whichever selected option covers
    /// it.
    #[test]
    fn several_replacements_can_share_the_dependents() {
        let offers = vec![
            offer(Choice::Package("left".to_string()), &["one"]),
            offer(Choice::Package("right".to_string()), &["two"]),
        ];
        let selected = vec![
            Choice::Package("left".to_string()),
            Choice::Package("right".to_string()),
        ];

        let assigned = assign(&offers, &selected, &["one".to_string(), "two".to_string()]);

        assert_eq!(
            assigned,
            vec![
                Assignment {
                    dependent: "one".to_string(),
                    choice: Some(Choice::Package("left".to_string())),
                },
                Assignment {
                    dependent: "two".to_string(),
                    choice: Some(Choice::Package("right".to_string())),
                },
            ]
        );
    }

    /// Where two selected options both serve a dependent, the one the list put
    /// first wins -- so selecting a further option never moves a dependent
    /// that an earlier one was already covering.
    #[test]
    fn the_first_offer_that_serves_a_dependent_takes_it() {
        let offers = vec![
            offer(Choice::Official, &["one"]),
            offer(Choice::Package("also".to_string()), &["one"]),
        ];

        let assigned = assign(
            &offers,
            &[Choice::Official, Choice::Package("also".to_string())],
            &["one".to_string()],
        );
        assert_eq!(assigned[0].choice, Some(Choice::Official));

        let without = assign(
            &offers,
            &[Choice::Package("also".to_string())],
            &["one".to_string()],
        );
        assert_eq!(
            without[0].choice,
            Some(Choice::Package("also".to_string())),
            "and the later one takes it when the first is not selected"
        );
    }

    /// A dependent nothing selected can serve has no assignment: it still
    /// needs this package, and has to be removed or the removal does nothing.
    #[test]
    fn an_unserved_dependent_gets_nothing() {
        let offers = vec![offer(Choice::Package("some".to_string()), &["one"])];

        let assigned = assign(
            &offers,
            &[Choice::Package("some".to_string())],
            &["one".to_string(), "two".to_string()],
        );

        assert_eq!(
            assigned[1],
            Assignment {
                dependent: "two".to_string(),
                choice: None,
            }
        );
    }

    /// An offer that could serve a dependent does nothing until it is picked.
    #[test]
    fn an_unselected_offer_serves_nobody() {
        let offers = vec![offer(Choice::Package("some".to_string()), &["one"])];
        let assigned = assign(&offers, &[], &["one".to_string()]);
        assert_eq!(assigned[0].choice, None);
    }

    /// A dependent being removed is not pointed anywhere, even when a selected
    /// offer could serve it, and it no longer counts as needing the package.
    #[test]
    fn a_removed_dependent_is_neither_replaced_nor_unaccounted() {
        let offers = vec![offer(Choice::Package("some".to_string()), &["one"])];
        let dependents = ["one".to_string(), "two".to_string()];
        let selected = [Choice::Package("some".to_string())];

        let plan = removal_plan(&offers, &dependents, &selected, &[], None);
        assert_eq!(plan.unaccounted, 1, "two is served by nothing");
        assert_eq!(
            plan.replacements,
            vec![Replacement {
                dependent: "one".to_string(),
                choice: Choice::Package("some".to_string()),
            }]
        );

        let plan = removal_plan(&offers, &dependents, &selected, &dependents, None);
        assert_eq!(plan.unaccounted, 0);
        assert!(plan.replacements.is_empty(), "{plan:?}");
    }

    /// What goes is the walk's answer once it is in, and the ticked dependents
    /// alone until then.
    #[test]
    fn the_cascade_decides_what_goes_once_it_is_known() {
        let removing = ["one".to_string()];
        let walked = ["one".to_string(), "above-one".to_string()];

        let pending = removal_plan(&[], &removing, &[], &removing, None);
        assert_eq!(pending.doomed, removing);

        let known = removal_plan(&[], &removing, &[], &removing, Some(&walked));
        assert_eq!(known.doomed, walked);
    }

    /// Toggling keeps the rest in the order they were picked, which is the
    /// order offers are tried in.
    #[test]
    fn membership_toggles_without_reordering_the_rest() {
        let mut list = vec![1, 2, 3];
        set_member(&mut list, 2, false);
        assert_eq!(list, [1, 3]);
        set_member(&mut list, 2, true);
        assert_eq!(list, [1, 3, 2]);
        set_member(&mut list, 2, true);
        assert_eq!(list, [1, 3, 2], "adding twice keeps one");
    }
}
