//! One package: its state, what it depends on, and where it came from.
//!
//! Built around the question someone actually arrives with — "is this package
//! healthy, and if not what do I do" — rather than around the data model. The
//! header answers it; everything below is reference material.
//!
//! Deliberately not a list of builds. That was the old centrepiece, and it
//! buried what matters: for each architecture, the most recent build and — when
//! that one failed — the older build the repository still serves for it. The
//! full history is a click away.

use crate::api::{LoadError, client};
use crate::dates::RelativeDate;
use crate::format::{format_bytes, format_duration, now_secs};
use crate::listing::ViewParams;
use crate::platforms::PlatformChecklist;
use crate::routes::Route;
use crate::status::{BuildStatusBadge, StatusBadge};
use aurcache_client::{
    Build, ExtendedPackage, PackageFile, PackagePatch, PackageSource, Setting, SettingSource,
};
use aurcache_common::build_state::BuildState;
use dioxus::prelude::*;
use std::collections::{HashMap, HashSet};

/// How many builds to pull for the summary. Enough for a stable typical
/// duration without fetching a long history the page does not show.
const BUILD_SAMPLE: u64 = 20;

/// Whether this build's output is what the repository serves for its
/// architecture.
fn succeeded(build: &Build) -> bool {
    build.status == BuildState::Successful
}

/// The architectures this package has builds for, ordered the way the platform
/// picker lists them, with any architecture the package no longer targets (but
/// still has history for) after those.
fn platforms_of(builds: &[Build]) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    for build in builds {
        if !seen.iter().any(|p| p == &build.platform) {
            seen.push(build.platform.clone());
        }
    }
    // Stable sort: known architectures fall into picker order, and anything
    // unrecognised keeps the order it was first seen in, after them.
    seen.sort_by_key(|p| {
        crate::platforms::ALL
            .iter()
            .position(|a| a == p)
            .unwrap_or(crate::platforms::ALL.len())
    });
    seen
}

/// The most recent build on one architecture, whatever its outcome.
///
/// The API returns builds newest-first, but that is an ordering this screen
/// depends on for correctness, so it picks the maximum explicitly rather than
/// trusting position.
fn latest_on<'a>(builds: &'a [Build], platform: &str) -> Option<&'a Build> {
    builds
        .iter()
        .filter(|b| b.platform == platform)
        .max_by_key(|b| (b.start_time, b.number))
}

/// The newest successful build on one architecture — the one whose packages the
/// repository serves for it.
fn in_repo_on<'a>(builds: &'a [Build], platform: &str) -> Option<&'a Build> {
    builds
        .iter()
        .filter(|b| b.platform == platform && succeeded(b))
        .max_by_key(|b| (b.end_time, b.number))
}

/// How long a build of this package usually takes.
///
/// Median rather than mean: one pathological build (a machine that swapped, a
/// dependency that rebuilt the world) drags an average somewhere no build has
/// ever actually landed.
///
/// Only successful builds count. A failure's duration measures how quickly
/// something broke, which is a different quantity and usually much shorter.
fn typical_duration(builds: &[Build]) -> Option<std::time::Duration> {
    let mut durations: Vec<std::time::Duration> = builds
        .iter()
        .filter(|b| succeeded(b))
        .filter_map(Build::elapsed)
        .collect();

    if durations.is_empty() {
        return None;
    }
    durations.sort_unstable();
    // Even counts take the lower of the two middle values rather than
    // averaging: the result stays a duration some build actually took.
    Some(durations[(durations.len() - 1) / 2])
}

/// The package names this build produces.
///
/// A split package declares them; anything else produces one package named
/// after its pkgbase.
fn produced_names(pkg: &ExtendedPackage) -> Vec<String> {
    match &pkg.split_packages {
        Some(names) if !names.is_empty() => names.clone(),
        _ => vec![pkg.name.clone()],
    }
}

/// What the package page shows: the package, and its most recent builds.
struct PackagePage {
    package: ExtendedPackage,
    builds: Result<Vec<Build>, String>,
}

async fn load(pkgbase: String) -> Result<PackagePage, LoadError> {
    let client = client()?;

    // Issued together rather than one after the other: they share no data, so
    // running them in sequence would add one round trip's latency to the
    // other's for no reason. The browser is the executor; this is concurrency,
    // not threads.
    let (package, builds) = futures_util::future::join(
        client.get_package(&pkgbase),
        client.list_builds(&aurcache_client::BuildQuery {
            pkgbase: Some(pkgbase.clone()),
            limit: Some(BUILD_SAMPLE),
            ..Default::default()
        }),
    )
    .await;

    // A failed build query should not lose the package itself: the build
    // summary is secondary, and the rest of the page is still worth showing.
    Ok(PackagePage {
        package: package?,
        builds: builds.map_err(|e| e.to_string()),
    })
}

#[component]
pub fn Package(pkgbase: String) -> Element {
    // `use_reactive` so the fetch follows the route. Navigating between two
    // packages reuses this component -- same route, different parameter -- and
    // a resource whose closure captured the old name simply never re-runs: the
    // URL changes, no request is made, and the previous package stays on
    // screen looking like the one that was clicked.
    let mut data = use_resource(use_reactive(&pkgbase, load));

    // A package that is not here is one you might want to add: a stale link
    // (a log entry, a bookmark) lands on the add page with the name searched.
    let notice = crate::notice::use_notice();
    use_effect(use_reactive(&pkgbase, move |pkgbase| {
        if matches!(&*data.read(), Some(Err(LoadError::NotFound))) {
            crate::notice::redirect(
                notice,
                Route::PackageAdd { q: pkgbase.clone() },
                crate::notice::Level::Info,
                format!("No package called {pkgbase} is tracked here. Search the AUR to add it."),
            );
        }
    }));

    // Refresh while this package or one of its recent builds is still in
    // flight, so a build finishing updates the status and the build summary
    // without a reload; a slow tick otherwise as a catch-all.
    let busy = matches!(&*data.read_unchecked(), Some(Ok(PackagePage { package, builds }))
        if package.status.is_in_progress() || builds.as_ref().is_ok_and(|builds| builds.iter().any(|b| b.status.is_in_progress())));
    crate::poll::use_poll(data, busy);

    rsx! {
        match &*data.read_unchecked() {
            // Missing is on its way elsewhere; see the redirect above.
            None | Some(Err(LoadError::NotFound)) => rsx! {
                div { class: "flex justify-center p-16",
                    span { class: "loading loading-spinner loading-lg" }
                }
            },
            Some(Err(e)) => rsx! {
                div { class: "alert alert-error", span { "Could not load {pkgbase}: {e}" } }
            },
            Some(Ok(PackagePage { package: pkg, builds })) => rsx! {
                div { class: "space-y-4",
                    PackageHeader {
                        pkg: pkg.clone(),
                        trail: vec![],
                        on_rebuilt: move |()| data.restart(),
                    }
                    // The sidebar starts level with the builds card rather than
                    // below it, so the builds card is only as wide as it needs
                    // and the space beside it is used.
                    //
                    // A fixed 26rem sidebar rather than a fraction: its content
                    // is URLs and versions, which wrap badly, and a fraction of
                    // a wide viewport is more than they need while a fraction
                    // of a narrow one is less.
                    div { class: "grid grid-cols-1 lg:grid-cols-[minmax(0,1fr)_26rem] gap-4 items-start",
                        // Dependency lists are unbounded — thirty entries is
                        // ordinary — so they take the flexible column.
                        div { class: "space-y-4 min-w-0",
                            match builds {
                                Ok(builds) => rsx! { BuildSummary { pkgbase: pkg.name.clone(), builds: builds.clone() } },
                                Err(error) => rsx! { div { class: "alert alert-error", "Could not load build history: {error}" } },
                            }
                            Relations {
                                pkg: pkg.clone(),
                                on_changed: move |()| data.restart(),
                            }
                            crate::screens::logs::RecentActivity {
                                about: aurcache_client::PackageRef::from(pkg.name.as_str()).into(),
                            }
                            // Last in this column rather than full width
                            // under both: it is the one irreversible action
                            // here, and it still has no business sitting
                            // beside Rebuild where people click without
                            // reading -- but spanning the page put it below a
                            // sidebar that had already run out, leaving the
                            // space beside it doing nothing.
                            //
                            // A package that is only here as a dependency
                            // cannot be removed by clearing a flag that is
                            // already clear -- its dependents have to stop
                            // needing it first. One that was asked for keeps
                            // the plain Remove, which clears the flag and
                            // leaves it as a dependency; removing it then is
                            // the second step, from this same card.
                            if !pkg.directly_requested && !pkg.dependents.is_empty() {
                                ReplaceAndRemoveCard {
                                    pkgbase: pkg.name.clone(),
                                    dependents: pkg.dependents.clone(),
                                    on_changed: move |()| data.restart(),
                                }
                            } else {
                                RemoveCard {
                                    pkgbase: pkg.name.clone(),
                                    dependents: pkg.dependents.len(),
                                    on_changed: move |()| data.restart(),
                                }
                            }
                        }
                        div { class: "space-y-4 min-w-0",
                            SourceCard { pkg: pkg.clone() }
                            BuildConfigCard {
                                pkg: pkg.clone(),
                                on_changed: move |()| data.restart(),
                            }
                            ArtifactsCard { pkg: pkg.clone() }
                        }
                    }
                }
            },
        }
    }
}

#[component]
pub fn PackageHeader(
    pkg: ExtendedPackage,
    trail: Vec<(String, Option<Route>)>,
    on_rebuilt: EventHandler<()>,
) -> Element {
    let description = pkg.description.clone();
    let pkgbase = pkg.name.clone();
    // The AUR page for this package, when it has one. The header is the one
    // place every package-scoped page shares, so the link lives here rather
    // than in the sidebar's Source card.
    let aur_url = match &pkg.package_source {
        PackageSource::Aur(aur) => Some(aur.aur_url.clone()),
        _ => None,
    };

    rsx! {
        div { class: "card bg-base-100 shadow-xl",
            div { class: "card-body",
                div { class: "flex flex-wrap items-start gap-3",
                    div { class: "min-w-0 flex-1",
                        // The trail *is* the heading, rather than a small copy
                        // of it above: the package name appeared twice
                        // otherwise. Ancestors are muted so the page you are on
                        // still reads as the title.
                        //
                        // Laid out as ordinary inline text, not as a flex
                        // row: a baseline-aligned flex row takes its baseline
                        // from whichever item has the taller ascent, and the
                        // package name is monospace while the crumbs are not.
                        //
                        // `leading-8` is what actually lands "Packages" on the
                        // same pixel as the identical word on the package list.
                        // There the heading's 28px line box is centred in this
                        // 32px row, so it starts 2px down; here the line box is
                        // the full 32px and starts at 0, and the extra
                        // half-leading puts the baseline in the same place. Let
                        // the box size itself instead and it comes out 29px
                        // tall — the monospace name widens it — which centres
                        // to a half pixel and rounds the whole heading up by
                        // one.
                        div { class: "flex items-center min-h-8",
                            h1 { class: "card-title block leading-8 break-words",
                                Link {
                                    class: "opacity-60 link-hover",
                                    to: Route::Packages { view: ViewParams::default(), q: String::new() },
                                    "Packages"
                                }
                                span { class: "opacity-30 mx-2", "/" }
                                if trail.is_empty() {
                                    span { class: "font-mono", "{pkg.name}" }
                                } else {
                                    Link {
                                        class: "font-mono opacity-60 link-hover",
                                        to: Route::Package { pkgbase: pkg.name.clone() },
                                        "{pkg.name}"
                                    }
                                    for (index, (label, route)) in trail.iter().enumerate() {
                                        span { key: "sep-{index}", class: "opacity-30 mx-2", "/" }
                                        match route.clone() {
                                            Some(route) => rsx! {
                                                Link {
                                                    key: "{index}",
                                                    class: "opacity-60 link-hover",
                                                    to: route,
                                                    "{label}"
                                                }
                                            },
                                            None => rsx! { span { key: "{index}", "{label}" } },
                                        }
                                    }
                                }
                            }
                        }
                        div { class: "flex items-center gap-3 flex-wrap mt-1",
                            StatusBadge { status: pkg.status, out_of_date: pkg.out_of_date }
                            if pkg.has_patch {
                                span { class: "badge badge-warning badge-sm", "patched" }
                            }
                            if !pkg.directly_requested {
                                span {
                                    class: "badge badge-outline badge-sm",
                                    title: "Pulled in as a dependency, not requested directly",
                                    "dependency"
                                }
                            }
                            if let Some(url) = aur_url {
                                a {
                                    class: "link link-primary text-sm",
                                    href: "{url}",
                                    target: "_blank",
                                    rel: "noopener noreferrer",
                                    "AUR ↗"
                                }
                            }
                        }
                        if let Some(description) = description {
                            p { class: "opacity-70 mt-1", "{description}" }
                        }
                        VersionLine { pkg }
                    }
                    // Rebuilding starts a new build rather than describing one,
                    // so the button sits with the package — in the header every
                    // package-scoped page shares — instead of beside the builds
                    // list on one of them.
                    div { class: "shrink-0",
                        RebuildButton { pkgbase, on_changed: on_rebuilt }
                    }
                }
            }
        }
    }
}

/// What is built versus what is available upstream.
///
/// Rendered as one line rather than two labelled fields, because the only
/// reason to show both is the comparison between them.
#[component]
fn VersionLine(pkg: ExtendedPackage) -> Element {
    let built = pkg.built_version.clone();
    let upstream = pkg.upstream_version;

    rsx! {
        div { class: "mt-2 flex items-center gap-2 text-sm font-mono flex-wrap",
            match built {
                Some(built) => rsx! { span { "{built}" } },
                // Never built: say so rather than showing a bare dash, which
                // reads as missing data.
                None => rsx! { span { class: "opacity-60 italic font-sans", "never built" } },
            }
            match upstream {
                Some(upstream) => rsx! {
                    span { class: "opacity-40", "·" }
                    span { class: "opacity-70", "upstream {upstream}" }
                },
                None => rsx! {
                    span { class: "opacity-40", "·" }
                    span { class: "opacity-60 italic font-sans", "upstream not checked yet" }
                },
            }
        }
    }
}

/// The most recent build on each architecture, and how long a build usually
/// takes.
///
/// One row per architecture rather than a single "latest": a package builds for
/// each architecture on its own — a PKGBUILD can compile cleanly for x86_64 and
/// fail to cross-compile for armv7h — and a single latest build shows whichever
/// architecture ran last while saying nothing about the rest. When an
/// architecture's most recent build failed, a second line names the older build
/// its repository still serves.
#[component]
fn BuildSummary(pkgbase: String, builds: Vec<Build>) -> Element {
    let now = now_secs();
    let typical = typical_duration(&builds);

    // (label, build) in display order: each architecture's most recent build,
    // and after a failed one the build the repository still serves for it.
    let mut rows: Vec<(String, Build)> = Vec::new();
    for platform in platforms_of(&builds) {
        let Some(newest) = latest_on(&builds, &platform) else {
            continue;
        };
        rows.push((platform.clone(), newest.clone()));
        // A second line only when that most recent build did not succeed:
        // otherwise it *is* what the repository serves, so `in_repo_on` would
        // return the same build and the row would just be noise.
        if !succeeded(newest)
            && let Some(repo) = in_repo_on(&builds, &platform)
        {
            rows.push(("↳ in repo".to_string(), repo.clone()));
        }
    }

    rsx! {
        div { class: "card bg-base-100 shadow-xl",
            div { class: "card-body py-4",
                div { class: "flex items-center gap-3 flex-wrap",
                    Link {
                        class: "card-title text-base link-hover",
                        to: Route::PackageBuilds { pkgbase },
                        "Builds"
                    }
                    if let Some(typical) = typical {
                        span { class: "text-sm opacity-60",
                            "typically {format_duration(Some(typical))}"
                        }
                    }
                }

                if rows.is_empty() {
                    p { class: "opacity-60 text-sm", "This package has never been built." }
                } else {
                    div { class: "divide-y divide-base-300",
                        for (label, entry) in rows.iter() {
                            BuildRow {
                                // No index prefix: an insert above would
                                // restamp every key below it, which is what
                                // keys exist to survive. Platform and number
                                // are unique across a package's builds.
                                key: "{label}-{entry.number}",
                                label: label.clone(),
                                entry: entry.clone(),
                                now,
                            }
                        }
                    }
                }
            }
        }
    }
}

/// One line of the build summary: an architecture and its most recent build, or
/// "↳ in repo" and the build still served for it after that one failed.
///
/// "in repo" rather than "latest successful": the label says what it means for
/// the reader — this is the version pacman will install right now.
///
/// The prop is `entry`, not `build`: Dioxus generates a props builder whose own
/// `build()` finalises it, so a prop of that name is ambiguous at the call site.
#[component]
fn BuildRow(label: String, entry: Build, now: i64) -> Element {
    rsx! {
        Link {
            class: "flex items-center gap-3 py-2 hover:bg-base-200 px-2 -mx-2 rounded flex-wrap",
            to: Route::Build { pkgbase: entry.pkg_name.clone(), number: entry.number },
            span { class: "text-sm opacity-60 w-20 shrink-0", "{label}" }
            span { class: "font-mono text-sm", "{entry.number}" }
            BuildStatusBadge { status: entry.status }
            span { class: "font-mono text-sm opacity-70", "{entry.version}" }
            div { class: "flex-1" }
            span { class: "text-sm opacity-60", RelativeDate { ts: entry.start_time, now } }
            span { class: "font-mono text-sm opacity-60 w-16 text-right",
                {format_duration(entry.elapsed())}
            }
        }
    }
}

#[component]
fn Relations(pkg: ExtendedPackage, on_changed: EventHandler<()>) -> Element {
    let name = pkg.name.clone();
    let ExtendedPackage {
        dependencies,
        dependents,
        ..
    } = pkg;

    rsx! {
        RelationList {
            title: "Dependencies",
            empty: "Nothing — this package builds on its own.",
            items: dependencies,
            // Only dependencies gate this package's build. A dependent that is
            // unsatisfied is waiting on *this* package, which is its problem to
            // display, not a reason to flag anything here.
            show_blocking: true,
            // Only this side is editable: an edge belongs to the package that
            // declares it, so a dependent's dependency is changed from the
            // dependent's own page.
            replace_for: Some(name),
            on_changed,
        }
        RelationList {
            title: "Dependents",
            empty: "Nothing depends on this package.",
            items: dependents,
            show_blocking: false,
            replace_for: None,
            on_changed,
        }
    }
}

#[component]
fn RelationList(
    title: String,
    empty: String,
    items: Vec<aurcache_client::PackageDependency>,
    show_blocking: bool,
    replace_for: Option<String>,
    on_changed: EventHandler<()>,
) -> Element {
    let blocking = items.iter().filter(|item| !item.satisfied).count();

    rsx! {
        div { class: "card bg-base-100 shadow-xl",
            div { class: "card-body",
                h2 { class: "card-title text-base",
                    "{title}"
                    if !items.is_empty() {
                        span { class: "badge badge-sm badge-neutral", "{items.len()}" }
                    }
                    if show_blocking && blocking > 0 {
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
                                        // On the loop child, where the diff needs it:
                                        // the key inside `RelationRow`'s own template
                                        // cannot tell sibling rows apart.
                                        key: "{item.name}",
                                        item: item.clone(),
                                        show_blocking,
                                        replace_for: replace_for.clone(),
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
    show_blocking: bool,
    replace_for: Option<String>,
    on_changed: EventHandler<()>,
) -> Element {
    let blocking = show_blocking && !item.satisfied;
    let mut replacing = use_signal(|| false);
    let pkgbase = item.name.clone();
    let replace_target = replace_for;
    // The row handler's own copy: it has to move something in, while the
    // name below still needs the original.
    let nav_pkgbase = pkgbase.clone();

    rsx! {
        tr { key: "{item.name}", class: "hover cursor-pointer",
            // The whole row is the target, but the name stays a real link so
            // the address is copyable, middle-click opens a tab, and keyboard
            // users have something to focus — none of which a bare row
            // handler gives.
            onclick: move |_| {
                navigator().push(Route::Package { pkgbase: nav_pkgbase.clone() });
            },
            td {
                Link {
                    // Monospace, matching the package page's heading: a pkgbase
                    // is an identifier and reads as one.
                    //
                    // Not `link link-primary`: when everything in the row
                    // navigates, underlining one cell implies the rest does
                    // not.
                    class: "font-mono",
                    to: Route::Package { pkgbase: pkgbase.clone() },
                    // Otherwise the click reaches the row too and pushes the
                    // same route twice, leaving a duplicate history entry.
                    onclick: move |e: MouseEvent| e.stop_propagation(),
                    "{pkgbase}"
                }
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
            if replace_target.is_some() {
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
        if let Some(dependent) = replace_target {
            if replacing() {
                ReplaceDependencyDialog {
                    dependent,
                    dependency: pkgbase,
                    on_close: move |()| replacing.set(false),
                    on_changed,
                }
            }
        }
    }
}

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
fn shared_candidates(
    per_dependent: &[(String, Vec<aurcache_client::DependencyCandidate>)],
) -> Vec<SharedCandidate> {
    // Indexed by pkgbase: the merge used to scan the whole Vec per
    // candidate, quadratic in dependents × candidates.
    let mut index: HashMap<&str, usize> = HashMap::new();
    let mut merged: Vec<(SharedCandidate, usize)> = Vec::new();
    for (dependent, candidates) in per_dependent {
        for (rank, candidate) in candidates.iter().enumerate() {
            match index.get(candidate.pkgbase.as_str()) {
                Some(&at) => {
                    let (shared, best_rank) = &mut merged[at];
                    shared.serves.push(dependent.clone());
                    *best_rank = (*best_rank).min(rank);
                }
                None => {
                    index.insert(candidate.pkgbase.as_str(), merged.len());
                    merged.push((
                        SharedCandidate {
                            pkgbase: candidate.pkgbase.clone(),
                            source: candidate.source,
                            serves: vec![dependent.clone()],
                        },
                        rank,
                    ));
                }
            }
        }
    }

    merged.sort_by(|(a, a_rank), (b, b_rank)| {
        b.serves
            .len()
            .cmp(&a.serves.len())
            .then_with(|| a_rank.cmp(b_rank))
            .then_with(|| a.pkgbase.cmp(&b.pkgbase))
    });
    merged.into_iter().map(|(shared, _)| shared).collect()
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
fn offers_for(loaded: &[(String, aurcache_client::DependencyOptions)]) -> Vec<Offer> {
    let mut offers = Vec::new();

    let official: Vec<String> = loaded
        .iter()
        .filter(|(_, options)| {
            // Every declared name, not merely one of them: a dependent whose
            // other name is unpublished would be left with a dependency
            // nothing satisfies.
            !options.official.is_empty() && options.official.len() == options.declared_names.len()
        })
        .map(|(dependent, _)| dependent.clone())
        .collect();
    if !official.is_empty() {
        offers.push(Offer {
            choice: Choice::Official,
            source: None,
            serves: official,
        });
    }

    offers.extend(
        shared_candidates(
            &loaded
                .iter()
                .map(|(dependent, options)| (dependent.clone(), options.candidates.clone()))
                .collect::<Vec<_>>(),
        )
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

/// Send one dependency somewhere else. `None` drops it.
async fn apply_replacement(
    dependent: String,
    dependency: String,
    replacement: Option<String>,
) -> Result<(), String> {
    match client() {
        Ok(client) => client
            .replace_dependency(&dependent, &dependency, replacement.as_deref())
            .await
            .map_err(|e| e.to_string()),
        Err(e) => Err(e),
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
                match client() {
                    Ok(client) => client
                        .dependency_options(&dependent, &dependency)
                        .await
                        .map_err(|e| e.to_string()),
                    Err(e) => Err(e),
                }
            }
        }
    });

    let mut busy = use_signal(|| false);
    let mut error = use_signal(|| Option::<String>::None);

    // Held as signals so the handler below captures nothing but `Copy` values
    // and can therefore be used from more than one place in the tree.
    let ends = use_signal(|| (dependent.clone(), dependency.clone()));
    let apply = move |replacement: Option<String>| {
        let (dependent, dependency) = ends();
        spawn(async move {
            busy.set(true);
            error.set(None);
            let outcome = apply_replacement(dependent, dependency, replacement).await;
            busy.set(false);
            match outcome {
                Ok(()) => {
                    on_changed.call(());
                    on_close.call(());
                }
                Err(e) => error.set(Some(e)),
            }
        });
    };

    rsx! {
        div {
            class: "modal modal-open",
            role: "dialog",
            aria_modal: "true",
            aria_label: "Replace dependency",
            div { class: "modal-box max-w-2xl",
                h3 { class: "font-bold text-lg", "Replace {dependency}" }
                p { class: "text-sm opacity-70 pt-1",
                    "Changes what {dependent} is built against. "
                    "The choice sticks: resolution prefers the dependency a package already has."
                }

                if let Some(message) = error() {
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
                                        disabled: busy(),
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
                                            busy: busy(),
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
                        disabled: busy(),
                        onclick: move |_| on_close.call(()),
                        "Cancel"
                    }
                }
            }
            button {
                class: "modal-backdrop",
                disabled: busy(),
                onclick: move |_| on_close.call(()),
                "Close"
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

#[component]
fn SourceCard(pkg: ExtendedPackage) -> Element {
    rsx! {
        div { class: "card bg-base-100 shadow-xl",
            div { class: "card-body",
                h2 { class: "card-title text-base", "Source" }
                // Where the source comes from. The descriptive fields below
                // are read from the checkout, so they are the same for an AUR
                // package and a git one.
                match &pkg.package_source {
                    // No Origin row for AUR packages: the AUR link lives in
                    // the page header, which every package-scoped page shares.
                    PackageSource::Aur(_) => rsx! {},
                    PackageSource::AurNotFound(_) => rsx! {
                        div { class: "alert alert-warning text-sm mb-2",
                            span { "No longer found on the AUR." }
                        }
                    },
                    PackageSource::Git(spec) => rsx! {
                        Field { label: "Origin", span { "Git" } }
                        Field { label: "URL",
                            match browsable_url(&spec.url) {
                                Some(href) => rsx! {
                                    a {
                                        class: "link link-primary font-mono text-xs break-all",
                                        href: "{href}",
                                        target: "_blank",
                                        rel: "noopener noreferrer",
                                        "{spec.url}"
                                    }
                                },
                                // An SSH remote is not a page a browser can
                                // open, so it stays text rather than becoming
                                // a link that goes nowhere.
                                None => rsx! {
                                    span { class: "font-mono text-xs break-all", "{spec.url}" }
                                },
                            }
                        }
                        Field { label: "Ref", span { class: "font-mono text-xs", "{spec.r#ref}" } }
                        if !spec.subfolder.is_empty() {
                            Field { label: "Subfolder",
                                span { class: "font-mono text-xs", "{spec.subfolder}" }
                            }
                        }
                    },
                    PackageSource::Upload(_) => rsx! {
                        Field { label: "Origin", span { "Uploaded archive" } }
                    },
                }

                if let Some(maintainer) = pkg.maintainer.clone() {
                    Field { label: "Maintainer", span { "{maintainer}" } }
                }
                if let Some(licenses) = pkg.licenses.clone() {
                    Field { label: "Licenses", span { "{licenses}" } }
                }
                if let Some(url) = pkg.project_url.clone() {
                    Field { label: "Upstream",
                        a { class: "link link-primary break-all", href: "{url}",
                            target: "_blank", rel: "noopener noreferrer", "{url}" }
                    }
                }
                if matches!(&pkg.package_source, PackageSource::Aur(aur) if aur.aur_flagged_outdated) {
                    div { class: "alert alert-warning text-sm mt-2",
                        span { "Flagged out of date on the AUR." }
                    }
                }
            }
        }
    }
}

#[component]
fn BuildConfigCard(pkg: ExtendedPackage, on_changed: EventHandler<()>) -> Element {
    let entry = use_resource(use_reactive(&pkg.name, move |pkgbase| async move {
        client()?
            .settings(Some(&pkgbase))
            .await
            .map_err(|error| error.to_string())
    }));
    rsx! {
        div { class: "card bg-base-100 shadow-xl",
            div { class: "card-body",
                h2 { class: "card-title text-base", "Build config" }
                PlatformField {
                    pkgbase: pkg.name.clone(),
                    selected: pkg.selected_platforms.clone(),
                    on_changed,
                }
                BuildFlagsField {
                    pkgbase: pkg.name.clone(),
                    // A package with no flags stores an empty string, which the
                    // API splits on `;` into one empty flag rather than none.
                    // Left in, it renders as a chip with no label.
                    flags: pkg
                        .selected_build_flags
                        .clone()
                        .unwrap_or_default()
                        .into_iter()
                        .filter(|flag| !flag.trim().is_empty())
                        .collect(),
                    on_changed,
                }
                PersistBuildDirField { key: "persist-{pkg.name}", pkgbase: pkg.name.clone(), entry }
                ArtifactSizeField { key: "artifact-{pkg.name}", pkgbase: pkg.name.clone(), entry }
                // Editing the sources is what creates or clears the package's
                // patch, so the button says whether there is one rather than a
                // row of its own saying so beside it.
                Link {
                    class: "btn btn-sm btn-block mt-2",
                    to: Route::PackageSource { pkgbase: pkg.name.clone(), path: vec![] },
                    if pkg.has_patch { "Edit sources (patch exists)" } else { "Edit sources" }
                }
                // The per-package makepkg.conf/pacman.conf overrides. They
                // need a page of their own — two full-height editors — and
                // without a way through it is reachable only by typing its URL.
                Link {
                    class: "btn btn-sm btn-block mt-2",
                    to: Route::PackageConfigFiles { pkgbase: pkg.name },
                    "Config files"
                }
            }
        }
    }
}

/// Whether this package keeps its build tree between builds.
///
/// A boolean with no Save step: toggling applies immediately, which is all the
/// setting warrants. The checkbox flips on click and a re-read after the write
/// keeps the row showing what the server actually holds, and where it now comes
/// from — a package override, or a value inherited from elsewhere.
///
/// Read through the settings sub-resource rather than the package object:
/// settings are fetched separately, and folding one of them into the package
/// payload would make every package request carry it.
#[component]
fn PersistBuildDirField(
    pkgbase: String,
    mut entry: Resource<Result<aurcache_client::ApplicationSettings, String>>,
) -> Element {
    use crate::setting_editor::{
        MutationOutcome, SettingEdit, SettingScope, use_editor_draft, use_setting_mutations,
    };
    let controller = use_setting_mutations();
    let busy = controller.busy;
    let error = move || (controller.status)().and_then(Result::err);
    let loaded = entry
        .read()
        .as_ref()
        .and_then(|result| result.as_ref().ok())
        .map(|settings| settings.persistent_builddir.value.to_string());
    let mut draft = use_editor_draft(loaded);
    let scope = SettingScope::Package(pkgbase);
    let save_scope = scope.clone();
    let set = move |enabled: bool| {
        let scope = save_scope.clone();
        async move {
            let value = enabled.to_string();
            draft.with_mut(|draft| draft.edit(value.clone()));
            if controller
                .apply(
                    &scope,
                    SettingEdit::set(Setting::PersistentBuilddir, value).editing(draft),
                )
                .await
                == MutationOutcome::Applied
            {
                entry.restart();
            }
        }
    };
    let reset = move |_| {
        let scope = scope.clone();
        async move {
            if controller
                .apply(
                    &scope,
                    SettingEdit::reset(Setting::PersistentBuilddir).editing(draft),
                )
                .await
                == MutationOutcome::Applied
            {
                entry.restart();
            }
        }
    };

    rsx! {
        div { class: "flex gap-2 py-1 text-sm items-center",
            span { class: "opacity-60 w-32 shrink-0", "Persist build dir" }
            div { class: "min-w-0 flex-1",
                match &*entry.read_unchecked() {
                    None => rsx! {
                        span { class: "loading loading-spinner loading-xs" }
                    },
                    Some(Err(e)) => rsx! {
                        span { class: "text-xs text-error", "Could not load the setting: {e}" }
                    },
                    Some(Ok(settings)) => {
                        let source = settings.persistent_builddir.source;
                        let locked = source == SettingSource::Env;
                        rsx! {
                            div { class: "flex items-center gap-2 flex-wrap",
                                input {
                                    r#type: "checkbox",
                                    class: "toggle toggle-primary",
                                    disabled: locked || busy(),
                                    checked: draft().text() == "true",
                                    onchange: move |e: FormEvent| {
                                        set(e.checked())
                                    },
                                }
                                if source == SettingSource::Package && !locked && !busy() {
                                    button {
                                        class: "btn btn-ghost btn-xs",
                                        title: "Discard this package's override and inherit the default again",
                                        onclick: reset,
                                        "Reset"
                                    }
                                }
                                if let Some(message) = error() {
                                    span { class: "text-xs text-error", "{message}" }
                                }
                            }
                            if locked
                                && let Some(name) = Setting::PersistentBuilddir.meta().env_name
                            {
                                p { class: "text-xs text-warning", "unset ${name} to allow control here" }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// The largest package file this package's builds may upload.
///
/// A server setting that resolves per package (see `Setting::MaxArtifactSize`),
/// so one outsized package can be allowed more without raising the limit for
/// all. Shown as the size it is written in (`20G`), with where the value comes
/// from; typing a new one offers Save, and a package override offers Reset.
#[component]
fn ArtifactSizeField(
    pkgbase: String,
    mut entry: Resource<Result<aurcache_client::ApplicationSettings, String>>,
) -> Element {
    use crate::setting_editor::{
        MutationOutcome, SettingEdit, SettingScope, use_editor_draft, use_setting_mutations,
    };
    let controller = use_setting_mutations();
    let busy = controller.busy;
    let error = move || (controller.status)().and_then(Result::err);
    let loaded = entry
        .read()
        .as_ref()
        .and_then(|result| result.as_ref().ok())
        .map(|settings| aurcache_common::units::format_size(settings.max_artifact_size.value));
    let mut draft = use_editor_draft(loaded);
    let scope = SettingScope::Package(pkgbase);
    let save_scope = scope.clone();
    let save = move |_| {
        let scope = save_scope.clone();
        async move {
            let value = draft().text().trim().to_owned();
            if controller
                .apply(
                    &scope,
                    SettingEdit::set(Setting::MaxArtifactSize, value).editing(draft),
                )
                .await
                == MutationOutcome::Applied
            {
                entry.restart();
            }
        }
    };
    let reset = move |_| {
        let scope = scope.clone();
        async move {
            if controller
                .apply(
                    &scope,
                    SettingEdit::reset(Setting::MaxArtifactSize).editing(draft),
                )
                .await
                == MutationOutcome::Applied
            {
                entry.restart();
            }
        }
    };

    rsx! {
        div { class: "flex gap-2 py-1 text-sm items-center",
            span { class: "opacity-60 w-32 shrink-0", "Max artifact size" }
            div { class: "min-w-0 flex-1",
                match &*entry.read_unchecked() {
                    None => rsx! { span { class: "loading loading-spinner loading-xs" } },
                    Some(Err(e)) => rsx! {
                        span { class: "text-xs text-error", "Could not load the setting: {e}" }
                    },
                    Some(Ok(settings)) => {
                        let current = aurcache_common::units::format_size(settings.max_artifact_size.value);
                        let source = settings.max_artifact_size.source;
                        // No lock for an environment variable: a package's own
                        // value outranks it (Package -> Env -> Global -> Default),
                        // so an override here still applies.
                        let changed = draft().text().trim() != current;
                        rsx! {
                            div { class: "flex items-center gap-2 flex-wrap",
                                input {
                                    r#type: "text",
                                    class: "input input-bordered input-xs w-24 font-mono",
                                    placeholder: "20G",
                                    disabled: busy(),
                                    value: draft().text(),
                                    oninput: move |e| draft.with_mut(|draft| draft.edit(e.value())),
                                }
                                span {
                                    class: "badge badge-ghost badge-sm",
                                    title: "Where this value comes from",
                                    match source {
                                        SettingSource::Package => "this package",
                                        SettingSource::Global => "global",
                                        SettingSource::Env => "environment",
                                        SettingSource::Default => "default",
                                    }
                                }
                                if changed {
                                    button {
                                        class: "btn btn-primary btn-xs",
                                        disabled: busy(),
                                        onclick: save,
                                        "Save"
                                    }
                                }
                                if source == SettingSource::Package && !busy() {
                                    button {
                                        class: "btn btn-ghost btn-xs",
                                        title: "Discard this package's limit and use the global one again",
                                        onclick: reset,
                                        "Reset"
                                    }
                                }
                            }
                            if let Some(message) = error() {
                                p { class: "text-xs text-error", "{message}" }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// What lands in the repository when this package builds.
#[component]
fn ArtifactsCard(pkg: ExtendedPackage) -> Element {
    let names = produced_names(&pkg);
    let split = pkg
        .split_packages
        .as_ref()
        .is_some_and(|names| names.len() > 1);

    // Once the package has built, the repository rows are the truth: they say
    // which of the declared names actually produced an artifact, on which
    // platform, and how big it is. Before the first build there are no rows, so
    // the declared names are all the page can show.
    let built = !pkg.files.is_empty();
    let total = total_size(&pkg.files);

    rsx! {
        div { class: "card bg-base-100 shadow-xl",
            div { class: "card-body",
                h2 { class: "card-title text-base",
                    "Artifacts"
                    if built {
                        span { class: "badge badge-sm badge-neutral", "{pkg.files.len()}" }
                    } else if split {
                        span { class: "badge badge-sm badge-neutral", "{names.len()}" }
                    }
                }
                ul { class: "divide-y divide-base-300",
                    if built {
                        for file in pkg.files.iter() {
                            // Arch and size first, at their natural width; the
                            // filename takes the rest and scrolls sideways
                            // rather than wrapping. In a 26rem column a
                            // `name-version-arch.pkg.tar.zst` is wider than the
                            // row, and wrapping it broke the small fields onto
                            // their own lines too ("30 MiB" split in two).
                            li { key: "{file.filename}", class: "py-2 flex items-center gap-2",
                                span { class: "badge badge-ghost badge-xs shrink-0", "{file.platform}" }
                                FileSize { file: file.clone() }
                                span {
                                    class: "font-mono text-xs flex-1 min-w-0 overflow-x-auto \
                                            whitespace-nowrap",
                                    title: "{file.filename}",
                                    "{file.filename}"
                                }
                            }
                        }
                    } else {
                        // Declared names, not artifacts: these are pkgbase /
                        // split-package names, short enough to sit on one line.
                        for name in names.iter() {
                            li { key: "{name}", class: "py-2 flex items-center gap-2",
                                NotTracked { what: "size" }
                                span { class: "font-mono text-sm break-all min-w-0", "{name}" }
                            }
                        }
                    }
                }
                if let Some(total) = total {
                    div { class: "flex items-baseline gap-2 pt-2 text-sm",
                        span { class: "opacity-60", "Total" }
                        span { class: "font-mono", {format_bytes(total)} }
                    }
                }
                if !built {
                    // Nothing is in the repository yet, so these are what the
                    // PKGBUILD declares rather than what exists.
                    p { class: "text-xs opacity-50 mt-2",
                        "Declared package names. Nothing has been built into the repository yet."
                    }
                }
            }
        }
    }
}

/// The same repository as a page a browser can open, if it is one.
///
/// Git remotes are not all web addresses. `git+ssh://git@github.com/...` is a
/// supported and documented source here, and so is `git@host:path`; neither is
/// something a browser can follow. Only http(s) remotes become links, so the
/// rest render as plain text instead of a link that goes nowhere.
///
/// The `git+` prefix is makepkg's way of marking a source as a git repository,
/// not part of the address.
fn browsable_url(raw: &str) -> Option<String> {
    let raw = raw.trim();
    let url = raw.strip_prefix("git+").unwrap_or(raw);
    (url.starts_with("https://") || url.starts_with("http://")).then(|| url.to_string())
}

/// Queue a rebuild of this package and land on the new build's page.
///
/// Disabled while in flight so a double click does not queue twice, and it
/// reports what happened rather than silently doing nothing. `update_package`
/// returns the build numbers it queued — one per platform — so the result is
/// shown rather than waited for: the destination build's page is the update.
#[component]
fn RebuildButton(pkgbase: String, on_changed: EventHandler<()>) -> Element {
    let mut busy = use_signal(|| false);
    let mut error = use_signal(|| Option::<String>::None);

    rsx! {
        div { class: "flex items-center gap-2",
            if let Some(message) = error() {
                span { class: "text-xs text-error", "{message}" }
            }
            button {
                class: "btn btn-primary btn-sm",
                disabled: busy(),
                onclick: move |_| {
                    let pkgbase = pkgbase.clone();
                    async move {
                        busy.set(true);
                        error.set(None);
                        let outcome = match client() {
                            Ok(client) => client
                                .update_package(&pkgbase, &aurcache_client::UpdatePackage {
                                    force: true,
                                })
                                .await
                                .map_err(|e| e.to_string()),
                            Err(e) => Err(e),
                        };
                        match outcome {
                            Err(e) => error.set(Some(e)),
                            Ok(ids) => {
                                on_changed.call(());
                                if let Some(number) = ids.into_iter().next() {
                                    navigator().push(Route::Build { pkgbase, number });
                                }
                            }
                        }
                        busy.set(false);
                    }
                },
                if busy() {
                    span { class: "loading loading-spinner loading-xs" }
                }
                "Rebuild"
            }
        }
    }
}

/// The platforms a package is built for, with an inline editor.
///
/// Changing this changes which dependencies are required — a PKGBUILD can
/// declare `depends_aarch64` separately — so the server re-resolves the
/// dependency graph on save. That is why the page reloads afterwards rather
/// than patching the field in place.
#[component]
pub fn PlatformField(
    pkgbase: String,
    selected: Vec<String>,
    on_changed: EventHandler<()>,
) -> Element {
    let mut editing = use_signal(|| false);
    let mut draft = use_signal(|| selected.clone());
    let mut busy = use_signal(|| false);
    let mut error = use_signal(|| Option::<String>::None);

    // Reset the draft each time the editor opens, so a cancelled edit does not
    // linger into the next one.
    let start = {
        let selected = selected.clone();
        move |_| {
            draft.set(selected.clone());
            error.set(None);
            editing.set(true);
        }
    };

    rsx! {
        div { class: "flex gap-2 py-1 text-sm",
            span { class: "opacity-60 w-24 shrink-0", "Platforms" }
            div { class: "min-w-0 flex-1",
                if editing() {
                    div { class: "flex flex-col gap-1",
                        PlatformChecklist {
                            selected: draft(),
                            onchange: move |next| draft.set(next),
                        }
                        if let Some(message) = error() {
                            span { class: "text-xs text-error", "{message}" }
                        }
                        div { class: "flex gap-2 pt-1",
                            button {
                                class: "btn btn-primary btn-xs",
                                // A package built for nothing would never build
                                // again, so saving an empty set is refused
                                // rather than accepted and puzzled over later.
                                disabled: busy() || draft().is_empty(),
                                onclick: {
                                    move |_| {
                                        let pkgbase = pkgbase.clone();
                                        async move {
                                            busy.set(true);
                                            error.set(None);
                                            let outcome = match client() {
                                                Ok(client) => client
                                                    .patch_package(&pkgbase, &aurcache_client::PackagePatch {
                                                        platforms: Some(draft()),
                                                        ..Default::default()
                                                    })
                                                    .await
                                                    .map_err(|e| e.to_string()),
                                                Err(e) => Err(e),
                                            };
                                            match outcome {
                                                Ok(()) => {
                                                    editing.set(false);
                                                    on_changed.call(());
                                                }
                                                Err(e) => error.set(Some(e)),
                                            }
                                            busy.set(false);
                                        }
                                    }
                                },
                                "Save"
                            }
                            button {
                                class: "btn btn-ghost btn-xs",
                                disabled: busy(),
                                onclick: move |_| editing.set(false),
                                "Cancel"
                            }
                        }
                    }
                } else {
                    div { class: "flex items-center gap-2 flex-wrap",
                        span { class: "font-mono text-xs", {selected.join(", ")} }
                        button {
                            class: "btn btn-ghost btn-xs",
                            onclick: start,
                            "Change"
                        }
                    }
                }
            }
        }
    }
}

/// A value the backend does not expose yet.
///
/// Rendered as an explicit marker rather than a blank or a dash: an empty cell
/// reads as "this package has no value", which is a different and wrong claim.
#[component]
fn NotTracked(what: String) -> Element {
    rsx! {
        span {
            class: "badge badge-ghost badge-xs opacity-50",
            title: "Not tracked yet: {what}",
            "—"
        }
    }
}

/// The combined size of every artifact, or `None` if any one of them is unknown.
///
/// All-or-nothing on purpose: summing only the known sizes would print a total
/// smaller than the parts it is made of, which reads as a bug rather than as
/// missing data. `Option`'s `Sum` gives exactly this — one `None` and the whole
/// total is `None`.
fn total_size(files: &[PackageFile]) -> Option<u64> {
    if files.is_empty() {
        return None;
    }
    files
        .iter()
        .map(|f| f.size.and_then(|s| u64::try_from(s).ok()))
        .sum()
}

/// One artifact's size, or the placeholder when it is not known.
///
/// Unknown means the row predates the size column and the file was already gone
/// when the startup backfill looked, so there is nothing to report -- rendering
/// it as `0 B` would claim the package file is empty.
#[component]
fn FileSize(file: PackageFile) -> Element {
    match file.size.and_then(|s| u64::try_from(s).ok()) {
        Some(bytes) => rsx! {
            // `shrink-0` + `whitespace-nowrap` so "30 MiB" keeps its one line
            // when the row is tight — it is the filename beside it that gives.
            span { class: "font-mono text-sm opacity-70 shrink-0 whitespace-nowrap",
                {format_bytes(bytes)}
            }
        },
        None => rsx! {
            NotTracked { what: "size" }
        },
    }
}

/// A label/value row inside a sidebar card.
#[component]
fn Field(label: String, children: Element) -> Element {
    rsx! {
        div { class: "flex gap-2 py-1 text-sm",
            span { class: "opacity-60 w-24 shrink-0", "{label}" }
            div { class: "min-w-0", {children} }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(
        number: i32,
        platform: &str,
        status: BuildState,
        start: Option<i64>,
        end: Option<i64>,
    ) -> Build {
        Build {
            number,
            pkg_name: "hello".to_string(),
            version: "1.0-1".to_string(),
            status,
            start_time: start,
            end_time: end,
            platform: platform.to_string(),
            size: None,
            peak_memory: None,
            disk_usage: None,
            kept: None,
            worker_name: None,
            log_size: None,
            waiting_reason: None,
        }
    }

    /// Each architecture's most recent build is found on its own: the newest
    /// build overall does not stand in for an architecture that has none.
    #[test]
    fn the_latest_build_is_per_architecture() {
        let builds = vec![
            sample(1, "x86_64", BuildState::Successful, Some(100), Some(150)),
            sample(2, "aarch64", BuildState::Failed, Some(200), Some(210)),
            sample(3, "x86_64", BuildState::Failed, Some(300), Some(310)),
        ];
        assert_eq!(latest_on(&builds, "x86_64").map(|b| b.number), Some(3));
        assert_eq!(latest_on(&builds, "aarch64").map(|b| b.number), Some(2));
        assert_eq!(latest_on(&builds, "armv7h"), None);
    }

    /// The repository serves the newest *successful* build for an architecture,
    /// which is exactly what its latest build is not once that one has failed.
    #[test]
    fn the_repo_build_is_the_newest_successful_one_for_its_architecture() {
        let builds = vec![
            sample(1, "x86_64", BuildState::Successful, Some(100), Some(150)),
            sample(2, "x86_64", BuildState::Successful, Some(200), Some(260)),
            sample(3, "x86_64", BuildState::Failed, Some(300), Some(310)),
        ];
        assert_eq!(in_repo_on(&builds, "x86_64").map(|b| b.number), Some(2));
        assert_ne!(
            latest_on(&builds, "x86_64").map(|b| b.number),
            in_repo_on(&builds, "x86_64").map(|b| b.number),
            "a failing latest build is the case the second row exists for"
        );
    }

    /// One architecture failing says nothing about what the repository serves
    /// for another.
    #[test]
    fn architectures_do_not_share_repo_state() {
        let builds = vec![
            sample(1, "x86_64", BuildState::Successful, Some(100), Some(150)),
            sample(2, "aarch64", BuildState::Failed, Some(200), Some(210)),
        ];
        assert_eq!(in_repo_on(&builds, "x86_64").map(|b| b.number), Some(1));
        assert_eq!(in_repo_on(&builds, "aarch64"), None);
    }

    /// Architectures are listed the way the platform picker lists them, whatever
    /// order builds arrive in; one the package no longer targets still shows if
    /// it has history, after the recognised ones.
    #[test]
    fn platforms_are_listed_in_picker_order() {
        let builds = vec![
            sample(1, "armv7h", BuildState::Successful, Some(100), Some(150)),
            sample(2, "x86_64", BuildState::Successful, Some(200), Some(260)),
            sample(3, "riscv64", BuildState::Failed, Some(300), Some(310)),
            sample(4, "aarch64", BuildState::Successful, Some(400), Some(460)),
            sample(5, "x86_64", BuildState::Failed, Some(500), Some(510)),
        ];
        assert_eq!(
            platforms_of(&builds),
            vec!["x86_64", "aarch64", "armv7h", "riscv64"]
        );
    }

    /// A package with no builds has no architectures to show.
    #[test]
    fn a_package_that_never_built_has_no_architectures() {
        assert!(platforms_of(&[]).is_empty());
    }

    /// Median, not mean: the 40-minute outlier must not become "typical".
    #[test]
    fn the_typical_duration_resists_an_outlier() {
        let builds = vec![
            sample(1, "x86_64", BuildState::Successful, Some(0), Some(60)),
            sample(2, "x86_64", BuildState::Successful, Some(0), Some(70)),
            sample(3, "x86_64", BuildState::Successful, Some(0), Some(2400)),
        ];
        assert_eq!(
            typical_duration(&builds),
            Some(std::time::Duration::from_secs(70))
        );
    }

    /// A failure measures how fast something broke, not how long a build takes.
    #[test]
    fn failed_builds_do_not_count_towards_the_typical_duration() {
        let builds = vec![
            sample(1, "x86_64", BuildState::Successful, Some(0), Some(600)),
            sample(2, "x86_64", BuildState::Failed, Some(0), Some(5)),
            sample(3, "x86_64", BuildState::Failed, Some(0), Some(5)),
        ];
        assert_eq!(
            typical_duration(&builds),
            Some(std::time::Duration::from_secs(600))
        );
    }

    /// An unfinished or never-run build contributes no duration, and a package
    /// with none at all has no typical time rather than a zero one.
    #[test]
    fn a_package_with_no_completed_builds_has_no_typical_duration() {
        assert_eq!(typical_duration(&[]), None);
        let running = vec![sample(1, "x86_64", BuildState::Active, Some(100), None)];
        assert_eq!(typical_duration(&running), None);
    }

    /// Every produced name is shown, and a package that declares none still
    /// produces one named after itself.
    #[test]
    fn produced_names_fall_back_to_the_pkgbase() {
        let mut pkg = package();
        assert_eq!(produced_names(&pkg), vec!["hello".to_string()]);

        pkg.split_packages = Some(vec!["hello".into(), "hello-docs".into()]);
        assert_eq!(
            produced_names(&pkg),
            vec!["hello".to_string(), "hello-docs".to_string()]
        );

        // An empty list is a declaration of nothing, which is not meaningful —
        // treat it as the un-split case rather than rendering an empty card.
        pkg.split_packages = Some(vec![]);
        assert_eq!(produced_names(&pkg), vec!["hello".to_string()]);
    }

    fn file(name: &str, size: Option<i64>) -> PackageFile {
        PackageFile {
            filename: name.to_string(),
            platform: "x86_64".to_string(),
            size,
        }
    }

    #[test]
    fn total_size_adds_every_artifact() {
        let files = vec![
            file("a.pkg.tar.zst", Some(1000)),
            file("b.pkg.tar.zst", Some(24)),
        ];
        assert_eq!(total_size(&files), Some(1024));
    }

    /// One unknown size makes the whole total unknown: a partial sum would
    /// render a total visibly smaller than the rows above it.
    #[test]
    fn one_unknown_size_makes_the_total_unknown() {
        let files = vec![
            file("a.pkg.tar.zst", Some(1000)),
            file("b.pkg.tar.zst", None),
        ];
        assert_eq!(total_size(&files), None);
    }

    /// A package that has never built has no artifacts, and no total to show —
    /// not a total of zero.
    #[test]
    fn a_package_with_no_artifacts_has_no_total() {
        assert_eq!(total_size(&[]), None);
    }

    fn package() -> ExtendedPackage {
        ExtendedPackage {
            name: "hello".to_string(),
            directly_requested: true,
            status: BuildState::Successful,
            out_of_date: false,
            built_version: Some("1.0-1".to_string()),
            selected_platforms: vec!["x86_64".to_string()],
            selected_build_flags: None,
            upstream_version: Some("1.0-1".to_string()),
            package_source: PackageSource::Git(aurcache_client::GitSourceSpec {
                url: "https://example.com/hello.git".to_string(),
                r#ref: "main".to_string(),
                subfolder: String::new(),
            }),
            split_packages: None,
            files: vec![],
            dependencies: vec![],
            dependents: vec![],
            has_patch: false,
            description: None,
            project_url: None,
            licenses: None,
            maintainer: None,
            first_submitted: None,
            last_modified: None,
        }
    }

    fn aur_package() -> ExtendedPackage {
        let mut pkg = package();
        pkg.package_source = PackageSource::Aur(aurcache_client::AurPackage {
            name: "hello".to_string(),
            aur_flagged_outdated: true,
            aur_url: "https://aur.archlinux.org/packages/hello".to_string(),
        });
        pkg
    }

    #[component]
    fn HeaderHarness(pkg: ExtendedPackage) -> Element {
        rsx! {
            PackageHeader { pkg, trail: vec![], on_rebuilt: move |_| {} }
        }
    }

    fn render_header(pkg: &ExtendedPackage) -> String {
        let mut dom =
            VirtualDom::new_with_props(HeaderHarness, HeaderHarnessProps { pkg: pkg.clone() });
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    #[component]
    fn SourceHarness(pkg: ExtendedPackage) -> Element {
        rsx! { SourceCard { pkg } }
    }

    fn render_source(pkg: &ExtendedPackage) -> String {
        let mut dom =
            VirtualDom::new_with_props(SourceHarness, SourceHarnessProps { pkg: pkg.clone() });
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    /// The header is the one place every package-scoped page shares, so it
    /// carries the AUR link — including on the builds list and the build
    /// detail page, which never had it.
    #[test]
    fn the_header_links_to_the_aur_when_there_is_one() {
        let html = render_header(&aur_package());
        assert!(
            html.contains("https://aur.archlinux.org/packages/hello"),
            "{html}"
        );
    }

    #[test]
    fn the_header_has_no_aur_link_for_a_git_package() {
        let html = render_header(&package());
        assert!(!html.contains("aur.archlinux.org"), "{html}");
    }

    /// The rebuild button moved up from the Builds card, so the build detail
    /// page — which has no Builds card — offers one too.
    #[test]
    fn the_rebuild_button_sits_in_the_header() {
        for pkg in [package(), aur_package()] {
            assert!(render_header(&pkg).contains("Rebuild"), "{pkg:?}");
        }
    }

    /// The link moved, not copied: the Source card keeps everything else it
    /// knew from the AUR — here the flagged-out-of-date warning — but the
    /// Origin row is gone.
    #[test]
    fn the_source_card_no_longer_links_to_the_aur() {
        let html = render_source(&aur_package());
        assert!(!html.contains("aur.archlinux.org"), "{html}");
        assert!(!html.contains("Origin"), "{html}");
        assert!(html.contains("Flagged out of date"), "{html}");
    }
}

#[cfg(test)]
mod shared_candidate_tests {
    use super::{Assignment, Choice, Offer, assign, offers_for, shared_candidates};
    use aurcache_client::{
        CandidateSource, DependencyCandidate, DependencyOptions, ReplacementVerdict,
    };

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

    /// Removing the package needs every dependent to stop wanting it, so the
    /// candidate that serves the most of them leads -- even where another one
    /// was ranked higher by the dependent that offered it.
    #[test]
    fn the_candidate_serving_the_most_dependents_leads() {
        let merged = shared_candidates(&[
            (
                "one".to_string(),
                vec![candidate("narrow"), candidate("wide")],
            ),
            ("two".to_string(), vec![candidate("wide")]),
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
        let merged = shared_candidates(&[(
            "one".to_string(),
            vec![candidate("zebra"), candidate("alpha")],
        )]);

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
            (
                "one".to_string(),
                vec![candidate("first"), candidate("second")],
            ),
            (
                "two".to_string(),
                vec![candidate("second"), candidate("first")],
            ),
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
            ("wants-git".to_string(), options(&["git"], &["git"])),
            ("wants-git-git".to_string(), options(&["git-git"], &[])),
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
        let offers = offers_for(&[(
            "one".to_string(),
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
            ("one".to_string(), published),
            ("two".to_string(), wide.clone()),
            ("three".to_string(), wide),
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
}

#[cfg(test)]
mod url_tests {
    use super::browsable_url;

    #[test]
    fn web_remotes_become_links() {
        assert_eq!(
            browsable_url("https://github.com/user/repo.git").as_deref(),
            Some("https://github.com/user/repo.git")
        );
        // `git+` marks the source as a repository; it is not part of the
        // address, and a browser would choke on it.
        assert_eq!(
            browsable_url("git+https://github.com/user/repo").as_deref(),
            Some("https://github.com/user/repo")
        );
        assert_eq!(
            browsable_url("http://example.com/r.git").as_deref(),
            Some("http://example.com/r.git")
        );
    }

    /// SSH remotes are supported sources here — the docs use
    /// `git+ssh://git@github.com/EpicGames/UnrealEngine` — but they are not
    /// pages. A link to one is worse than no link.
    #[test]
    fn ssh_remotes_do_not_become_links() {
        for raw in [
            "git+ssh://git@github.com/EpicGames/UnrealEngine",
            "ssh://git@example.com/repo.git",
            "git@github.com:user/repo.git",
            "file:///srv/local.git",
        ] {
            assert_eq!(browsable_url(raw), None, "{raw}");
        }
    }

    /// A scheme that merely contains "http" is not an http URL.
    #[test]
    fn only_a_real_http_scheme_counts() {
        assert_eq!(browsable_url("nothttps://example.com"), None);
        assert_eq!(browsable_url(""), None);
    }
}

/// The makepkg flags this package builds with, as chips.
///
/// Free-form rather than a fixed set: they are passed to makepkg, which has far
/// more of them than a checklist would be honest about. Every edit saves the
/// whole list, because that is what the endpoint takes — there is no
/// add-one/remove-one operation to mirror.
#[component]
fn BuildFlagsField(pkgbase: String, flags: Vec<String>, on_changed: EventHandler<()>) -> Element {
    let mut draft = use_signal(String::new);
    let mut busy = use_signal(|| false);
    let mut error = use_signal(|| Option::<String>::None);

    let current = use_signal(|| flags.clone());
    let current_pkgbase = use_signal(|| pkgbase.clone());
    // Follow the props: this component stays mounted when the route moves
    // between packages, so the signals need to track the current package.
    use_effect(use_reactive(&flags, move |flags| {
        let mut current = current;
        current.set(flags);
    }));
    use_effect(use_reactive(&pkgbase, move |pkgbase: String| {
        let mut current_pkgbase = current_pkgbase;
        current_pkgbase.set(pkgbase);
    }));

    let save = move |next: Vec<String>| async move {
        busy.set(true);
        error.set(None);
        let outcome = match client() {
            Ok(client) => client
                .patch_package(
                    &current_pkgbase(),
                    &PackagePatch {
                        build_flags: Some(next),
                        ..Default::default()
                    },
                )
                .await
                .map_err(|e| e.to_string()),
            Err(e) => Err(e),
        };
        busy.set(false);
        match outcome {
            Ok(()) => {
                draft.set(String::new());
                on_changed.call(());
            }
            Err(e) => error.set(Some(e)),
        }
    };

    // Adding a flag already present would save a list with a duplicate in it,
    // which makepkg would then see twice.
    let entered = draft().trim().to_string();
    let can_add = !entered.is_empty() && !current().contains(&entered) && !busy();

    let add = move |()| async move {
        let entered = draft().trim().to_string();
        if entered.is_empty() || current().contains(&entered) {
            return;
        }
        let mut next = current();
        next.push(entered);
        save(next).await;
    };

    rsx! {
        div { class: "flex gap-2 py-1 text-sm",
            span { class: "opacity-60 w-24 shrink-0", "Flags" }
            div { class: "min-w-0 flex-1 flex flex-col gap-2",
                if current().is_empty() {
                    span { class: "opacity-60 text-xs italic",
                        "No build flags. makepkg runs with its own defaults."
                    }
                } else {
                    div { class: "flex flex-wrap gap-1",
                        for flag in current() {
                            span {
                                key: "{flag}",
                                class: "badge badge-outline gap-1 font-mono text-xs",
                                "{flag}"
                                button {
                                    class: "opacity-60 hover:opacity-100",
                                    disabled: busy(),
                                    aria_label: "Remove {flag}",
                                    onclick: {
                                        move |_| {
                                            let flag = flag.clone();
                                            async move {
                                                let next = current()
                                                    .into_iter()
                                                    .filter(|f| *f != flag)
                                                    .collect();
                                                save(next).await;
                                            }
                                        }
                                    },
                                    "✕"
                                }
                            }
                        }
                    }
                }

                div { class: "flex gap-2",
                    input {
                        r#type: "text",
                        class: "input input-bordered input-xs font-mono w-48",
                        placeholder: "--nocheck",
                        value: "{draft}",
                        disabled: busy(),
                        oninput: move |e| draft.set(e.value()),
                        onkeydown: move |e: KeyboardEvent| async move {
                            if e.key() == Key::Enter {
                                add(()).await;
                            }
                        },
                    }
                    button {
                        class: "btn btn-xs",
                        disabled: !can_add,
                        onclick: move |_| add(()),
                        "Add"
                    }
                }

                if let Some(message) = error() {
                    span { class: "text-xs text-error", "{message}" }
                }
            }
        }
    }
}

/// Removing the package from the repository.
///
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
fn ReplaceAndRemoveCard(
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
    // `seen` answers membership; `order` keeps the breadth-first order the
    // old `Vec`-as-a-set version returned. A name is marked seen when it is
    // queued, so every frontier is already free of duplicates and of anything
    // an earlier level visited.
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
    let name = use_signal(|| pkgbase.clone());
    let names: Vec<String> = dependents.iter().map(|d| d.name.clone()).collect();
    let dependent_names = use_signal(|| names.clone());

    // One request per dependent, all in flight together. They are asked
    // separately because they are answered separately -- two packages needing
    // this one can declare different names for it and hold different
    // constraints -- but nothing about them is sequential.
    let plan = use_resource(move || {
        let pkgbase = name();
        let dependents = dependent_names();
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
                .map(|(dependent, answer)| {
                    answer
                        .map(|options| (dependent.clone(), options))
                        .map_err(|e| format!("{dependent}: {e}"))
                })
                .collect::<Result<Vec<_>, String>>()
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

    let apply = move |assignments: Vec<(String, Choice)>, doomed: Vec<String>| {
        let pkgbase = name();
        spawn(async move {
            busy.set(true);
            errors.set(Vec::new());
            let mut failed = Vec::new();
            let steps = assignments.len() + doomed.len();
            let mut step = 0;

            let client = match client() {
                Ok(client) => client,
                Err(e) => {
                    busy.set(false);
                    progress.set(None);
                    errors.set(vec![e]);
                    return;
                }
            };

            // In sequence: each one runs a collection on the way out, and two
            // of those racing would each be deciding what is still reachable
            // while the other changed it.
            for (dependent, choice) in &assignments {
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
            for dependent in doomed.iter().rev() {
                step += 1;
                progress.set(Some(format!("{step}/{steps} — removing {dependent}")));
                if let Err(e) = client.delete_package(dependent).await {
                    failed.push(format!("{dependent}: {e}"));
                }
            }

            busy.set(false);
            progress.set(None);
            if failed.is_empty() {
                // Nothing needs it any more, so the collection after the last
                // edit took it. Its page would now be a 404.
                navigator().push(Route::Packages {
                    view: ViewParams::default(),
                    q: String::new(),
                });
            } else {
                errors.set(failed);
                on_changed.call(());
            }
        });
    };

    rsx! {
        div {
            class: "modal modal-open",
            role: "dialog",
            aria_modal: "true",
            aria_label: "Replace and remove",
            div { class: "modal-box max-w-4xl",
                h3 { class: "font-bold text-lg", "Remove {pkgbase}" }

                for message in errors() {
                    div { class: "alert alert-error text-sm mt-3", span { "{message}" } }
                }

                match &*plan.read_unchecked() {
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
                            loaded.iter().map(|(name, _)| name.clone()).collect();
                        let total = dependents.len();
                        let assignments = assign(&offers, &selected(), &dependents);

                        let doomed = match &*cascade.read_unchecked() {
                            Some(Ok(closure)) => closure.clone(),
                            _ => removing(),
                        };
                        // Every dependent has to be accounted for. One left
                        // neither pointed elsewhere nor removed still needs
                        // this package, and then removing it does nothing --
                        // which is the button this card exists to replace.
                        let unaccounted = assignments
                            .iter()
                            .filter(|assignment| {
                                assignment.choice.is_none()
                                    && !removing().contains(&assignment.dependent)
                            })
                            .count();
                        let ready = unaccounted == 0;

                        let to_apply: Vec<(String, Choice)> = assignments
                            .iter()
                            .filter(|assignment| !removing().contains(&assignment.dependent))
                            .filter_map(|assignment| {
                                let choice = assignment.choice.clone()?;
                                Some((assignment.dependent.clone(), choice))
                            })
                            .collect();

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
                                                            let mut current = selected();
                                                            current.retain(|c| c != &choice);
                                                            if on {
                                                                current.push(choice.clone());
                                                            }
                                                            selected.set(current);
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
                                                        let mut current = removing();
                                                        current.retain(|d| d != &dependent);
                                                        if on {
                                                            current.push(dependent.clone());
                                                        }
                                                        removing.set(current);
                                                    }
                                                },
                                            }
                                        }
                                    }
                                }
                            }

                            // What the ticked removals really cost. A dependent
                            // has dependents of its own, and they cannot be left
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
                                    onclick: move |_| apply(to_apply.clone(), doomed.clone()),
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
            button {
                class: "modal-backdrop",
                disabled: busy(),
                onclick: move |_| on_close.call(()),
                "Close"
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
fn RemoveCard(pkgbase: String, dependents: usize, on_changed: EventHandler<()>) -> Element {
    let mut confirming = use_signal(|| false);
    let mut busy = use_signal(|| false);
    let mut error = use_signal(|| Option::<String>::None);

    let remove = {
        let pkgbase = pkgbase.clone();
        move |_| {
            let pkgbase = pkgbase.clone();
            async move {
                busy.set(true);
                error.set(None);
                let outcome = match client() {
                    Ok(client) => client
                        .delete_package(&pkgbase)
                        .await
                        .map_err(|e| e.to_string()),
                    Err(e) => Err(e),
                };
                busy.set(false);
                match outcome {
                    Ok(()) => {
                        confirming.set(false);
                        if dependents > 0 {
                            // The package stayed on as a dependency, so going
                            // to the list would say nothing about it. Refresh
                            // in place: the card becomes the replacement one.
                            on_changed.call(());
                        } else {
                            // The package may no longer exist, so going back to
                            // it would land on an error page.
                            navigator().push(Route::Packages {
                                view: ViewParams::default(),
                                q: String::new(),
                            });
                        }
                    }
                    Err(e) => error.set(Some(e)),
                }
            }
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
                if let Some(message) = error() {
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

        div {
            class: if confirming() { "modal modal-open" } else { "modal" },
            role: "dialog",
            aria_modal: "true",
            aria_label: "Confirm removal",
            div { class: "modal-box",
                h3 { class: "font-bold text-lg", "Remove {pkgbase}?" }
                p { class: "text-sm opacity-70 pt-2",
                    if dependents > 0 {
                        "It stops being a requested package and stays as a dependency for the packages that \
                         need it. Nothing is deleted; replacing and removing it is still open to you from this card."
                    } else {
                        "Nothing depends on it. It and its build history are deleted, and so is anything that \
                         was only here as its dependency. This cannot be undone."
                    }
                }
                div { class: "modal-action",
                    button {
                        class: "btn btn-sm",
                        disabled: busy(),
                        onclick: move |_| confirming.set(false),
                        "Cancel"
                    }
                    button {
                        class: "btn btn-error btn-sm",
                        disabled: busy(),
                        onclick: remove,
                        if busy() {
                            span { class: "loading loading-spinner loading-xs" }
                        }
                        "Remove"
                    }
                }
            }
            button {
                class: "modal-backdrop",
                disabled: busy(),
                onclick: move |_| confirming.set(false),
                aria_label: "Cancel removal",
                "Close"
            }
        }
    }
}
