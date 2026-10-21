//! The landing page: how the instance is doing, at a glance.

use super::logs::EntryText;
use crate::dates::AbsoluteDate;
use crate::format::{format_duration, format_size};
use crate::listing::{SortDir, SortKey, ViewParams};
use crate::routes::Route;
use crate::status::{BuildStatusBadge, StatusBadge};
use aurcache_client::{
    Build, GraphDataPoint, ListStats, LogEntry, LongBuild, OutOfDateSlice, QueueSlice, Severity,
    SimplePackage,
};
use aurcache_common::api::stats::LONGEST_WINDOW_DAYS;
use aurcache_common::build_state::BuildState;
use dioxus::prelude::*;
use dioxus_charts::LineChart;

#[component]
pub fn Dashboard() -> Element {
    let stats = use_resource(|| async move {
        crate::api::client()?
            .stats()
            .await
            .map_err(|e| e.to_string())
    });
    let graph = use_resource(|| async move {
        crate::api::client()?
            .graph()
            .await
            .map_err(|e| e.to_string())
    });
    let dashboard = use_resource(|| async move {
        crate::api::client()?
            .dashboard()
            .await
            .map_err(|e| e.to_string())
    });

    // The landing page is somewhere to leave open; a slow tick keeps the
    // totals and the chart roughly current without a reload. No "busy" input —
    // nothing here is worth a fast poll.
    crate::poll::use_poll(stats, false);
    crate::poll::use_poll(graph, false);
    crate::poll::use_poll(dashboard, false);

    // The warnings sit beside the chart at the top rather than buried in the
    // grid below: both load on their own requests, so neither moves the other
    // when it lands, and the wide-short chart never needed the full width.
    let problems = match &*dashboard.read_unchecked() {
        None => rsx! { SkeletonCard { title: "Recent problems" } },
        Some(Err(_)) => rsx! { SectionError { section: "recent problems" } },
        Some(Ok(view)) => rsx! {
            RecentProblemsCard { problems: view.problems.clone() }
        },
    };

    rsx! {
        div { class: "space-y-4",
            match &*stats.read_unchecked() {
                None => rsx! {
                    div { class: "flex justify-center p-8",
                        span { class: "loading loading-spinner loading-lg" }
                    }
                },
                Some(Err(e)) => rsx! {
                    div { class: "alert alert-error", span { "Could not load statistics: {e}" } }
                },
                Some(Ok(stats)) => rsx! {
                    StatTiles { stats: stats.clone() }
                },
            }

            div { class: "grid gap-4 lg:grid-cols-2 items-start",
                div { class: "card bg-base-100 shadow-xl",
                    div { class: "card-body",
                        div { class: "flex items-baseline gap-4 flex-wrap",
                            h2 { class: "card-title text-base", "Builds per month" }
                            div { class: "flex items-center gap-3 text-xs opacity-70",
                                span { class: "flex items-center gap-1",
                                    span { class: "inline-block w-3 h-0.5 bg-primary" }
                                    "started"
                                }
                                span { class: "flex items-center gap-1",
                                    span { class: "inline-block w-3 h-0.5 bg-success" }
                                    "succeeded"
                                }
                            }
                        }
                        match &*graph.read_unchecked() {
                            None => rsx! {
                                div { class: "flex justify-center p-8",
                                    span { class: "loading loading-spinner loading-lg" }
                                }
                            },
                            Some(Err(e)) => rsx! {
                                div { class: "alert alert-error", span { "Could not load the graph: {e}" } }
                            },
                            Some(Ok(points)) => rsx! {
                                BuildsChart { points: points.clone() }
                            },
                        }
                    }
                }
                {problems}
            }

            match &*dashboard.read_unchecked() {
                // Skeletons in the cards' full shape, so the page does not
                // jump when the first response lands. The problems card
                // skeleton sits beside the chart above, with its card.
                None => rsx! {
                    DashboardGrid {
                        left_top: rsx! { SkeletonCard { title: "Recent packages" } },
                        right_top: rsx! { SkeletonCard { title: "Recent builds" } },
                        left_attention: rsx! { SkeletonCard { title: "Failed packages" } },
                        right_attention: rsx! { SkeletonCard { title: "Out of date" } },
                        left_doing: rsx! { SkeletonCard { title: "Stuck queue" } },
                        left_slow: rsx! { SkeletonCard { title: "Largest packages" } },
                        right_slow: rsx! { SkeletonCard { title: "Longest builds" } },
                    }
                },
                Some(Err(e)) => rsx! {
                    div { class: "alert alert-error",
                        span { "Could not load the dashboard: {e}" }
                    }
                },
                Some(Ok(view)) => rsx! {
                    DashboardGrid {
                        left_top: rsx! {
                            RecentPackagesCard { packages: view.recent_packages.clone() }
                        },
                        right_top: rsx! {
                            RecentBuildsCard { builds: view.recent_builds.clone() }
                        },
                        left_attention: rsx! {
                            FailedPackagesCard { packages: view.failed.clone() }
                        },
                        right_attention: rsx! {
                            OutOfDateCard { slice: view.out_of_date.clone() }
                        },
                        left_doing: rsx! {
                            StuckQueueCard { queue: view.queue.clone() }
                        },
                        left_slow: rsx! {
                            LargestPackagesCard { packages: view.largest.clone() }
                        },
                        right_slow: rsx! {
                            LongestBuildsCard { builds: view.longest.clone() }
                        },
                    }
                },
            }
        }
    }
}

/// One card's chrome: the title itself links to the full list, like the
/// "Builds" title on the package page.
#[component]
fn Card(title: String, to: Route, children: Element) -> Element {
    rsx! {
        div { class: "card bg-base-100 shadow-xl",
            div { class: "card-body",
                Link {
                    class: "card-title text-base link-hover w-fit",
                    to: to,
                    "{title}"
                }
                {children}
            }
        }
    }
}

/// A healthy attention card, collapsed to one line: a success-toned check
/// and what is not happening.
#[component]
fn CollapsedCard(text: String) -> Element {
    rsx! {
        div { class: "card bg-base-100 shadow-xl",
            div { class: "card-body py-3 flex-row items-center gap-2",
                span { class: "text-success font-bold", "✓" }
                span { class: "text-sm", "{text}" }
            }
        }
    }
}

/// A card's full shape in skeleton blocks, shown while its section loads.
/// The title stays as screen-reader text so the loading region is named.
#[component]
fn SkeletonCard(title: String) -> Element {
    rsx! {
        div { class: "card bg-base-100 shadow-xl",
            div { class: "card-body",
                span { class: "sr-only", "{title} loading" }
                div { class: "skeleton h-5 w-32" }
                for _ in 0..5 {
                    div { class: "skeleton h-4 w-full mt-2" }
                }
            }
        }
    }
}

/// One failed dashboard section: an inline error, while the rest of the page
/// renders normally.
#[component]
fn SectionError(section: String) -> Element {
    rsx! {
        div { class: "alert alert-error",
            span { "Could not load {section}" }
        }
    }
}

/// "Stuck queue · 7 queued".
fn queue_title(depth: u64) -> String {
    format!("Stuck queue · {depth} queued")
}

/// "Longest builds · 30 days".
fn longest_title() -> String {
    format!("Longest builds · {LONGEST_WINDOW_DAYS} days")
}

/// The out-of-date footer when something rebuilds on its own, if anything.
fn handled_text(handled: u64) -> Option<String> {
    if handled > 0 {
        Some(format!("{handled} rebuilding on their own"))
    } else {
        None
    }
}

/// Why a queued build is still waiting: its worker reason, or — for a build
/// held on dependencies, which has no worker reason — the dependency hold.
fn queue_reason(build: &Build) -> Option<String> {
    if let Some(reason) = &build.waiting_reason {
        Some(reason.to_string())
    } else if build.status == BuildState::WaitingForDeps {
        Some("waiting for dependencies".to_string())
    } else {
        None
    }
}

#[component]
fn RecentPackagesCard(packages: Option<Vec<SimplePackage>>) -> Element {
    let Some(packages) = packages else {
        return rsx! { SectionError { section: "recent packages" } };
    };
    if packages.is_empty() {
        return rsx! {
            Card {
                title: "Recent packages",
                to: Route::Packages { view: ViewParams::default(), q: String::new() },
                div {
                    "No packages yet. "
                    Link {
                        class: "link link-primary",
                        to: Route::PackageAdd { q: String::new() },
                        "Add one →"
                    }
                }
            }
        };
    }
    rsx! {
        Card {
            title: "Recent packages",
            to: Route::Packages { view: ViewParams::default(), q: String::new() },
            div { class: "divide-y divide-base-300",
                for pkg in packages {
                    Link {
                        key: "{pkg.name}",
                        class: "py-2 px-2 -mx-2 rounded flex justify-between items-center gap-2 even:bg-base-200/50 hover:bg-base-200",
                        to: Route::Package { pkgbase: pkg.name.clone() },
                        span { class: "text-sm font-medium truncate", "{pkg.name}" }
                        StatusBadge { status: pkg.status, out_of_date: pkg.out_of_date }
                    }
                }
            }
        }
    }
}

#[component]
fn RecentBuildsCard(builds: Option<Vec<Build>>) -> Element {
    let Some(builds) = builds else {
        return rsx! { SectionError { section: "recent builds" } };
    };
    if builds.is_empty() {
        return rsx! {
            Card {
                title: "Recent builds",
                to: Route::Builds { view: ViewParams::default(), q: String::new() },
                div {
                    "No builds yet. "
                    Link {
                        class: "link link-primary",
                        to: Route::Builds { view: ViewParams::default(), q: String::new() },
                        "View builds →"
                    }
                }
            }
        };
    }
    rsx! {
        Card {
            title: "Recent builds",
            to: Route::Builds { view: ViewParams::default(), q: String::new() },
            div { class: "divide-y divide-base-300",
                for build in builds {
                    Link {
                        key: "{build.pkg_name} #{build.number}",
                        class: "py-2 px-2 -mx-2 rounded flex justify-between items-center gap-2 even:bg-base-200/50 hover:bg-base-200",
                        to: Route::Build {
                            pkgbase: build.pkg_name.clone(),
                            number: build.number,
                        },
                        span { class: "text-sm truncate",
                            span { class: "font-medium", "{build.pkg_name}" }
                            span { class: "opacity-60", " #{build.number} · {build.version}" }
                        }
                        span { class: "flex items-center gap-2 shrink-0",
                            BuildStatusBadge { status: build.status }
                            AbsoluteDate { ts: build.start_time }
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn FailedPackagesCard(packages: Option<Vec<SimplePackage>>) -> Element {
    let Some(packages) = packages else {
        return rsx! { SectionError { section: "failed packages" } };
    };
    if packages.is_empty() {
        return rsx! { CollapsedCard { text: "No failed packages" } };
    }
    rsx! {
        Card {
            title: "Failed packages",
            to: Route::Packages {
                view: ViewParams::with_status(BuildState::Failed),
                q: String::new(),
            },
            div { class: "divide-y divide-base-300",
                for pkg in packages {
                    Link {
                        key: "{pkg.name}",
                        class: "py-2 px-2 -mx-2 rounded flex justify-between items-center gap-2 even:bg-base-200/50 hover:bg-base-200",
                        to: Route::Package { pkgbase: pkg.name.clone() },
                        span { class: "text-sm font-medium truncate", "{pkg.name}" }
                        span { class: "text-sm opacity-60 truncate",
                            "{pkg.built_version.as_deref().unwrap_or(\"—\")}"
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn OutOfDateCard(slice: Option<OutOfDateSlice>) -> Element {
    let Some(slice) = slice else {
        return rsx! { SectionError { section: "out-of-date packages" } };
    };
    if slice.needs_hand.is_empty() {
        let text = match handled_text(slice.handled) {
            Some(count) => format!("Nothing needs a hand · {count}"),
            None => "Nothing needs a hand".to_string(),
        };
        return rsx! { CollapsedCard { text: text } };
    }
    rsx! {
        Card {
            title: "Out of date",
            to: Route::Packages {
                view: ViewParams::with_out_of_date(),
                q: String::new(),
            },
            div { class: "divide-y divide-base-300",
                for pkg in slice.needs_hand {
                    Link {
                        key: "{pkg.name}",
                        class: "py-2 px-2 -mx-2 rounded flex justify-between items-center gap-2 even:bg-base-200/50 hover:bg-base-200",
                        to: Route::Package { pkgbase: pkg.name.clone() },
                        span { class: "text-sm font-medium truncate", "{pkg.name}" }
                        span { class: "text-sm opacity-60 truncate",
                            "{pkg.built_version.as_deref().unwrap_or(\"—\")} → {pkg.upstream_version.as_deref().unwrap_or(\"—\")}"
                        }
                    }
                }
            }
            if let Some(count) = handled_text(slice.handled) {
                div { class: "text-sm opacity-60 pt-2", "{count}" }
            }
        }
    }
}

#[component]
fn StuckQueueCard(queue: Option<QueueSlice>) -> Element {
    let Some(queue) = queue else {
        return rsx! { SectionError { section: "the build queue" } };
    };
    if queue.depth == 0 {
        return rsx! { CollapsedCard { text: "Nothing queued" } };
    }
    rsx! {
        Card {
            title: queue_title(queue.depth),
            to: Route::Builds {
                view: ViewParams::with_states(&[
                    BuildState::Enqueued,
                    BuildState::WaitingForDeps,
                ]),
                q: String::new(),
            },
            div { class: "divide-y divide-base-300",
                for build in queue.oldest {
                    Link {
                        key: "{build.pkg_name} #{build.number}",
                        class: "py-2 px-2 -mx-2 rounded flex justify-between items-center gap-2 even:bg-base-200/50 hover:bg-base-200",
                        to: Route::Build {
                            pkgbase: build.pkg_name.clone(),
                            number: build.number,
                        },
                        span { class: "text-sm truncate",
                            span { class: "font-medium", "{build.pkg_name}" }
                            span { class: "opacity-60", " #{build.number} · {build.platform}" }
                            if let Some(reason) = queue_reason(&build) {
                                span { class: "opacity-60", " · {reason}" }
                            }
                        }
                        span { class: "ml-auto text-sm opacity-60 shrink-0",
                            AbsoluteDate { ts: build.start_time }
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn RecentProblemsCard(problems: Option<Vec<LogEntry>>) -> Element {
    let Some(problems) = problems else {
        return rsx! { SectionError { section: "recent problems" } };
    };
    if problems.is_empty() {
        return rsx! {
            Card {
                title: "Recent problems",
                to: Route::Logs { view: ViewParams::for_logs(None, false, None) },
                div { class: "opacity-60", "No warnings or errors" }
            }
        };
    }
    rsx! {
        Card {
            title: "Recent problems",
            to: Route::Logs {
                view: ViewParams::for_logs(Some(Severity::Warning), false, None),
            },
            div { class: "divide-y divide-base-300",
                for entry in problems {
                    div { key: "{entry.id}", class: "py-2 flex gap-2 items-baseline",
                        match entry.severity {
                            Severity::Warning => rsx! {
                                span { class: "badge badge-warning badge-sm shrink-0", "warning" }
                            },
                            Severity::Error => rsx! {
                                span { class: "badge badge-error badge-sm shrink-0", "error" }
                            },
                            Severity::Info => rsx! {},
                        }
                        span { class: "text-sm opacity-60 shrink-0",
                            AbsoluteDate { ts: Some(entry.timestamp) }
                        }
                        span { class: "text-sm line-clamp-2 min-w-0",
                            EntryText { entry: entry }
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn LargestPackagesCard(packages: Option<Vec<SimplePackage>>) -> Element {
    let Some(packages) = packages else {
        return rsx! { SectionError { section: "largest packages" } };
    };
    if packages.is_empty() {
        return rsx! {
            Card {
                title: "Largest packages",
                to: Route::Packages { view: ViewParams::default(), q: String::new() },
                div {
                    "No packages with artifacts yet. "
                    Link {
                        class: "link link-primary",
                        to: Route::Packages { view: ViewParams::default(), q: String::new() },
                        "View packages →"
                    }
                }
            }
        };
    }
    rsx! {
        Card {
            title: "Largest packages",
            to: Route::Packages { view: ViewParams::default(), q: String::new() },
            div { class: "divide-y divide-base-300",
                for pkg in packages {
                    Link {
                        key: "{pkg.name}",
                        class: "py-2 px-2 -mx-2 rounded flex justify-between items-center gap-2 even:bg-base-200/50 hover:bg-base-200",
                        to: Route::Package { pkgbase: pkg.name.clone() },
                        span { class: "text-sm font-medium truncate", "{pkg.name}" }
                        span { class: "text-sm opacity-60 shrink-0",
                            "{crate::format::format_size(pkg.total_size)}"
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn LongestBuildsCard(builds: Option<Vec<LongBuild>>) -> Element {
    let Some(builds) = builds else {
        return rsx! { SectionError { section: "longest builds" } };
    };
    if builds.is_empty() {
        return rsx! {
            Card {
                title: longest_title(),
                to: Route::Builds {
                    view: ViewParams::with_sort(SortKey::Duration, SortDir::Desc),
                    q: String::new(),
                },
                div {
                    "No successful builds yet. "
                    Link {
                        class: "link link-primary",
                        to: Route::Builds {
                            view: ViewParams::with_sort(SortKey::Duration, SortDir::Desc),
                            q: String::new(),
                        },
                        "View builds →"
                    }
                }
            }
        };
    }
    rsx! {
        Card {
            title: longest_title(),
            to: Route::Builds {
                view: ViewParams::with_sort(SortKey::Duration, SortDir::Desc),
                q: String::new(),
            },
            div { class: "divide-y divide-base-300",
                for item in builds {
                    Link {
                        key: "{item.build.pkg_name} #{item.build.number}",
                        class: "py-2 px-2 -mx-2 rounded flex justify-between items-center gap-2 even:bg-base-200/50 hover:bg-base-200",
                        to: Route::Build {
                            pkgbase: item.build.pkg_name.clone(),
                            number: item.build.number,
                        },
                        span { class: "text-sm truncate",
                            span { class: "font-medium", "{item.build.pkg_name}" }
                            span { class: "opacity-60", " #{item.build.number}" }
                            span { class: "badge badge-neutral badge-sm mx-2",
                                "{item.build.platform}"
                            }
                            span { class: "opacity-60",
                                "{crate::format::format_duration(item.build.elapsed())}"
                            }
                            if let Some(previous) = item.previous_secs {
                                span { class: "opacity-60", ", was {format_duration(Some(std::time::Duration::from_secs(previous)))}" }
                            }
                        }
                        span { class: "ml-auto text-sm opacity-60 shrink-0",
                            AbsoluteDate { ts: item.build.start_time }
                        }
                    }
                }
            }
        }
    }
}

/// The two columns of cards the dashboard stacks below the chart.
///
/// Each column is its own vertical stack, so a collapsed one-line card pulls
/// the card below it up instead of leaving its row's height behind, as paired
/// rows did. The warnings card is not one of them: it sits beside the chart
/// at the top.
#[component]
fn DashboardGrid(
    left_top: Element,
    right_top: Element,
    left_attention: Element,
    right_attention: Element,
    left_doing: Element,
    left_slow: Element,
    right_slow: Element,
) -> Element {
    rsx! {
        div { class: "grid gap-4 lg:grid-cols-2 items-start",
            div { class: "space-y-4 min-w-0",
                {left_top}
                {left_attention}
                {left_doing}
                {left_slow}
            }
            div { class: "space-y-4 min-w-0",
                {right_top}
                {right_attention}
                {right_slow}
            }
        }
    }
}

/// The share of finished builds that succeeded.
///
/// Over builds that *finished*: counting the ones still running as failures
/// would make a busy queue look like a broken server. `None` when nothing has
/// finished, because zero out of zero is not zero per cent.
fn success_rate(successful: u32, failed: u32) -> Option<f64> {
    // Added wide: the counters run over an unbounded history, so `u32 + u32`
    // would wrap at 2^32 builds into a wrong percentage.
    let finished = u64::from(successful) + u64::from(failed);
    (finished > 0).then(|| u64::from(successful) as f64 / finished as f64 * 100.0)
}

/// A rate as a percentage, or a dash.
fn format_rate(rate: Option<f64>) -> String {
    rate.map_or_else(|| "—".to_string(), |rate| format!("{rate:.0}%"))
}

/// The headline numbers.
#[component]
fn StatTiles(stats: ListStats) -> Element {
    // Added wide, like `success_rate` below: the behaviour on overflow should
    // not differ between two tiles of the same page.
    let all_packages = u64::from(stats.requested_packages) + u64::from(stats.dependency_packages);

    rsx! {
        div { class: "grid gap-4 grid-cols-2 lg:grid-cols-3 xl:grid-cols-5",
            StatTile {
                label: "Packages",
                value: "{stats.requested_packages}",
                // The requested count is the one people mean by "how many
                // packages do I have"; dependencies are along for the ride, so
                // they sit underneath rather than inflating the headline.
                note: "{all_packages} with dependencies",
                to: Route::Packages { view: ViewParams::default(), q: String::new() },
            }
            StatTile {
                label: "Builds",
                value: "{stats.total_builds}",
                note: "{stats.recent_builds} this week",
                to: Route::Builds { view: ViewParams::default(), q: String::new() },
            }
            StatTile {
                label: "Succeeded",
                value: format_rate(success_rate(stats.successful_builds, stats.failed_builds)),
                // A run of failures this week vanishes into a year of old
                // successes, which is exactly when it matters most.
                note: "{format_rate(success_rate(stats.recent_successful, stats.recent_failed))} this week",
            }
            StatTile {
                label: "Average build",
                value: format_duration(stats.avg_build_time.map(std::time::Duration::from_secs)),
            }
            StatTile {
                label: "Repository",
                value: format_size(stats.repo_size),
            }
        }
    }
}

/// One number, an optional second reading of it, and optionally a way to go
/// and look at what it counts.
#[component]
fn StatTile(label: String, value: String, note: Option<String>, to: Option<Route>) -> Element {
    let body = rsx! {
        div { class: "card-body p-4",
            div { class: "text-xs opacity-60", "{label}" }
            div { class: "text-2xl font-semibold", "{value}" }
            // Reserved whether or not there is one, so tiles with a note and
            // tiles without still line up in the row.
            div { class: "text-xs opacity-50 min-h-4",
                {note.unwrap_or_default()}
            }
        }
    };

    match to {
        Some(to) => rsx! {
            Link { class: "card bg-base-100 shadow-xl hover:bg-base-200 transition-colors", to, {body} }
        },
        None => rsx! {
            div { class: "card bg-base-100 shadow-xl", {body} }
        },
    }
}

/// Builds per month over the last year.
///
/// Static: `dioxus-charts` renders SVG and takes CSS classes, but has no hover
/// or event props, so there is no tooltip. Adding that upstream is its own
/// piece of work — see the note on the crate in `Cargo.toml`.
#[component]
fn BuildsChart(points: Vec<GraphDataPoint>) -> Element {
    if points.is_empty() {
        return rsx! {
            div { class: "alert", span { "No builds in the last year." } }
        };
    }

    // The server returns newest first; a time axis reads the other way.
    let mut points = points;
    points.sort_by_key(|p| (p.year, p.month));

    let labels: Vec<String> = points.iter().map(|p| month_label(p.month)).collect();
    // Two lines rather than a stack, which the crate cannot draw: total on top,
    // successes below it. The gap between them is the failures, which is what
    // anyone is actually looking for — and unlike a stack, both lines are read
    // against the same axis rather than one against a moving baseline.
    let started: Vec<f32> = points.iter().map(|p| p.count as f32).collect();
    let succeeded: Vec<f32> = points.iter().map(|p| p.successful as f32).collect();

    // The chart scales everything — text included — to its viewBox, and its
    // width defaults to 100%. Left alone it grows to whatever the card allows
    // and the axis labels come out enormous. A wide, short viewBox in a fixed
    // height gives a chart shaped like a chart.
    //
    // A phone gets a narrower viewBox of its own rather than the wide one
    // shrunk: scaled to a third, the axis labels came out 4px tall in a box
    // two-thirds empty.
    rsx! {
        // `chart-box` is what stops the chart growing with the window; see the
        // rule in index.html. Without it the fixed height below is advice the
        // svg is free to ignore, and does.
        div { class: "chart-box h-56 md:hidden",
            {line_chart(420, 300, vec![started.clone(), succeeded.clone()], labels.clone())}
        }
        div { class: "chart-box h-64 hidden md:block",
            {line_chart(900, 260, vec![started, succeeded], labels)}
        }
    }
}

/// The builds-per-month lines, drawn on a viewBox of the given size.
fn line_chart(
    viewbox_width: i32,
    viewbox_height: i32,
    series: Vec<Vec<f32>>,
    labels: Vec<String>,
) -> Element {
    rsx! {
        LineChart {
            width: "100%",
            height: "100%",
            viewbox_width,
            viewbox_height,
            padding_top: 20,
            padding_left: 50,
            padding_right: 30,
            padding_bottom: 45,
            series,
            labels,
            // Named explicitly rather than relying on the crate's default,
            // which is not the class its own docs imply: each series is
            // grouped as `{class_line}-{n}`, and the CSS below keys on it.
            class_line: "series",
            // Its own series labels are drawn past the last point, where
            // the viewBox clips them. The legend above the chart says the
            // same thing with room for it.
            show_line_labels: false,
            show_grid_ticks: true,
            show_dotted_grid: false,
            // Counts start at zero, and letting the axis float makes a
            // quiet month look like a collapse.
            lowest: 0.0,
            // Whole builds; halves on the axis are meaningless.
            max_ticks: 5,
            // The crate hardcodes a stroke as an SVG presentation
            // attribute, which any CSS rule outranks — so a class is
            // enough to put the lines back under the theme. It puts each
            // series in a `dx-chart-line-{n}` group, and the rules that
            // colour them separately live in index.html, since Tailwind
            // cannot express "a path inside that group".
            class_line_path: "chart-line",
            class_line_dot: "chart-line",
        }
    }
}

/// A month number as a short name.
///
/// Names rather than numbers because the axis is read at a glance, and `01`
/// against `12` is harder to place than `Jan` against `Dec`. English only, in
/// line with the rest of the UI.
fn month_label(month: i32) -> String {
    const NAMES: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    usize::try_from(month)
        .ok()
        .and_then(|m| m.checked_sub(1))
        .and_then(|m| NAMES.get(m))
        .map_or_else(|| month.to_string(), ToString::to_string)
}

#[cfg(test)]
mod tests {
    use super::{
        Build, BuildState, LONGEST_WINDOW_DAYS, OutOfDateSlice, QueueSlice, Severity, SortDir,
        SortKey, ViewParams,
    };
    use super::{
        DashboardGrid, DashboardGridProps, FailedPackagesCard, FailedPackagesCardProps,
        OutOfDateCard, OutOfDateCardProps, RecentProblemsCard, RecentProblemsCardProps,
        SkeletonCard, SkeletonCardProps, StatTiles, StatTilesProps, StuckQueueCard,
        StuckQueueCardProps, format_rate, handled_text, longest_title, month_label, queue_reason,
        queue_title, success_rate,
    };
    use aurcache_client::ListStats;
    use dioxus::prelude::*;

    #[test]
    fn months_read_as_names() {
        assert_eq!(month_label(1), "Jan");
        assert_eq!(month_label(12), "Dec");
        // Nothing the server should send, but a bad value must not panic or
        // silently show the wrong month.
        assert_eq!(month_label(0), "0");
        assert_eq!(month_label(13), "13");
        assert_eq!(month_label(-1), "-1");
    }

    /// The two linked tiles are absent from this markup: a `Link` renders
    /// nothing without a `Router` above it, and standing one up here would
    /// need the network the dashboard fetches from. That they appear at all is
    /// the browser test's job; what is worth testing here is the arithmetic.
    fn tiles(successful: u32, failed: u32, total: u32) -> String {
        let mut dom = VirtualDom::new_with_props(
            StatTiles,
            StatTilesProps {
                stats: ListStats {
                    total_builds: total,
                    successful_builds: successful,
                    failed_builds: failed,
                    // A deliberately worse week than the lifetime figures.
                    recent_builds: 4,
                    recent_successful: 1,
                    recent_failed: 3,
                    avg_build_time: Some(90),
                    repo_size: Some(3 * 1024 * 1024 * 1024),
                    requested_packages: 12,
                    dependency_packages: 4,
                    total_build_trend: 0.0,
                    avg_build_time_trend: 0.0,
                },
            },
        );
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    /// Over finished builds, not all of them. Counting the ones still running
    /// as failures would make a busy queue look like a broken server.
    #[test]
    fn the_success_rate_ignores_builds_still_running() {
        // Nine done, one still going: 8 of 9, not 8 of 10.
        assert_eq!(format_rate(success_rate(8, 1)), "89%");
    }

    /// A fresh instance has finished nothing; zero out of zero is not 0%.
    #[test]
    fn nothing_finished_means_no_rate_rather_than_zero() {
        assert_eq!(success_rate(0, 0), None);
        assert_eq!(format_rate(None), "—");
    }

    /// The lifetime rate and the weekly one are computed the same way but from
    /// different counts, and the tile shows both. A year of old successes
    /// hiding a bad week is the case this exists for.
    #[test]
    fn a_bad_week_shows_even_when_the_lifetime_rate_is_good() {
        let html = tiles(8, 1, 10);
        assert!(html.contains("89%"), "lifetime rate: {html}");
        // The fixture below gives the recent counts as 1 succeeded, 3 failed.
        assert!(html.contains("25% this week"), "weekly rate: {html}");
    }

    #[test]
    fn the_tiles_carry_their_numbers() {
        let html = tiles(8, 1, 10);
        assert!(html.contains("1m 30s"), "average build: {html}");
        assert!(html.contains("3.0 GiB"), "repository size: {html}");
    }

    /// A failed section renders an inline error while the rest would render.
    ///
    /// Only router-free states are asserted here: rows and title links
    /// need a `Router` above them, which panics outside the browser in a
    /// debug build — the browser suite covers those, like the logs list's.
    #[test]
    fn a_failed_section_is_an_inline_error() {
        let mut dom = VirtualDom::new_with_props(
            FailedPackagesCard,
            FailedPackagesCardProps { packages: None },
        );
        dom.rebuild_in_place();
        let html = dioxus_ssr::render(&dom);
        assert!(html.contains("Could not load failed packages"), "{html}");
        assert!(!html.contains("No failed packages"), "{html}");
    }

    /// Healthy attention cards collapse to one line instead of an empty card.
    #[test]
    fn empty_attention_cards_collapse() {
        let mut dom = VirtualDom::new_with_props(
            FailedPackagesCard,
            FailedPackagesCardProps {
                packages: Some(Vec::new()),
            },
        );
        dom.rebuild_in_place();
        let html = dioxus_ssr::render(&dom);
        assert!(html.contains("No failed packages"), "{html}");
        // Collapsed cards carry no title link to a list.
        assert!(!html.contains("View all"), "{html}");

        let mut dom = VirtualDom::new_with_props(
            StuckQueueCard,
            StuckQueueCardProps {
                queue: Some(QueueSlice {
                    depth: 0,
                    oldest: Vec::new(),
                }),
            },
        );
        dom.rebuild_in_place();
        let html = dioxus_ssr::render(&dom);
        assert!(html.contains("Nothing queued"), "{html}");
    }

    /// An empty out-of-date section keeps its handled count.
    #[test]
    fn an_empty_out_of_date_section_keeps_its_count() {
        let mut dom = VirtualDom::new_with_props(
            OutOfDateCard,
            OutOfDateCardProps {
                slice: Some(OutOfDateSlice {
                    needs_hand: Vec::new(),
                    handled: 4,
                }),
            },
        );
        dom.rebuild_in_place();
        let html = dioxus_ssr::render(&dom);
        assert!(html.contains("Nothing needs a hand"), "{html}");
        assert!(html.contains("4 rebuilding on their own"), "{html}");

        let mut dom = VirtualDom::new_with_props(
            OutOfDateCard,
            OutOfDateCardProps {
                slice: Some(OutOfDateSlice {
                    needs_hand: Vec::new(),
                    handled: 0,
                }),
            },
        );
        dom.rebuild_in_place();
        let html = dioxus_ssr::render(&dom);
        assert!(html.contains("Nothing needs a hand"), "{html}");
        assert!(!html.contains("rebuilding"), "{html}");
    }

    /// The problems empty state is a quiet line, not an empty card.
    ///
    /// Only the body is asserted here: the title is a link, which renders
    /// nothing without a `Router` above it — the browser suite covers the
    /// titles, like the rows.
    #[test]
    fn no_problems_says_so() {
        let mut dom = VirtualDom::new_with_props(
            RecentProblemsCard,
            RecentProblemsCardProps {
                problems: Some(Vec::new()),
            },
        );
        dom.rebuild_in_place();
        let html = dioxus_ssr::render(&dom);
        assert!(html.contains("No warnings or errors"), "{html}");
    }

    /// The grid is two independent columns, not paired rows: a collapsed
    /// one-line card pulls the card below it up instead of leaving its
    /// row's height behind.
    #[test]
    fn the_grid_stacks_two_independent_columns() {
        let mut dom = VirtualDom::new_with_props(
            DashboardGrid,
            DashboardGridProps {
                left_top: rsx! { span { "left-top" } },
                right_top: rsx! { span { "right-top" } },
                left_attention: rsx! { span { "left-attention" } },
                right_attention: rsx! { span { "right-attention" } },
                left_doing: rsx! { span { "left-doing" } },
                left_slow: rsx! { span { "left-slow" } },
                right_slow: rsx! { span { "right-slow" } },
            },
        );
        dom.rebuild_in_place();
        let html = dioxus_ssr::render(&dom);
        assert!(html.contains("lg:grid-cols-2 items-start"), "{html}");
        assert_eq!(html.matches("space-y-4").count(), 2, "{html}");
        // One column holds the left cards in order, the other the right ones.
        let mut positions = [
            "left-top",
            "left-attention",
            "left-doing",
            "left-slow",
            "right-top",
            "right-attention",
            "right-slow",
        ]
        .iter()
        .map(|marker| {
            html.find(marker)
                .unwrap_or_else(|| panic!("{marker} renders: {html}"))
        });
        let mut previous = positions.next().expect("a first card");
        for position in positions {
            assert!(previous < position, "{html}");
            previous = position;
        }
    }

    /// Skeletons hold the card's shape before the first response: a title
    /// bar and five row bars, no text yet.
    #[test]
    fn a_skeleton_holds_the_cards_shape() {
        let mut dom = VirtualDom::new_with_props(
            SkeletonCard,
            SkeletonCardProps {
                title: "Recent packages".to_string(),
            },
        );
        dom.rebuild_in_place();
        let html = dioxus_ssr::render(&dom);
        assert_eq!(html.matches("skeleton h-").count(), 6, "{html}");
        assert!(html.contains("Recent packages loading"), "{html}");
    }

    #[test]
    fn a_queued_build_names_what_holds_it() {
        use aurcache_client::WaitingReason;

        let reasoned = Build {
            number: 1,
            pkg_name: "hello".to_string(),
            version: "1.0".to_string(),
            status: BuildState::Enqueued,
            start_time: Some(100),
            end_time: None,
            platform: "x86_64".to_string(),
            size: None,
            peak_memory: None,
            disk_usage: None,
            kept: None,
            worker_name: None,
            log_size: None,
            waiting_reason: Some(WaitingReason::Arch {
                arch: "x86_64".to_string(),
            }),
        };
        let reason = queue_reason(&reasoned).expect("a reason");
        assert!(reason.contains("x86_64"), "{reason}");

        // Held on dependencies, with no worker reason of its own.
        let held = Build {
            status: BuildState::WaitingForDeps,
            waiting_reason: None,
            ..reasoned.clone()
        };
        assert_eq!(
            queue_reason(&held),
            Some("waiting for dependencies".to_string())
        );

        // Running normally, with nothing to explain.
        let plain = Build {
            status: BuildState::Active,
            waiting_reason: None,
            ..reasoned
        };
        assert_eq!(queue_reason(&plain), None);
    }

    #[test]
    fn card_titles_and_counts_read_plainly() {
        assert_eq!(queue_title(7), "Stuck queue · 7 queued");
        assert_eq!(
            longest_title(),
            format!("Longest builds · {LONGEST_WINDOW_DAYS} days")
        );
        assert_eq!(handled_text(0), None);
        assert_eq!(
            handled_text(4),
            Some("4 rebuilding on their own".to_string())
        );
    }

    /// Each card's title-link target, as a URL: the mapping SSR cannot render
    /// (links need a router) but the browser suite navigates.
    #[test]
    fn each_card_links_to_its_list() {
        use crate::routes::Route;

        let failed = Route::Packages {
            view: ViewParams::with_status(BuildState::Failed),
            q: String::new(),
        }
        .to_string();
        assert!(failed.contains("s=failed"), "{failed}");

        let outdated = Route::Packages {
            view: ViewParams::with_out_of_date(),
            q: String::new(),
        }
        .to_string();
        assert!(outdated.contains("s=outdated"), "{outdated}");

        let queued = Route::Builds {
            view: ViewParams::with_states(&[BuildState::Enqueued, BuildState::WaitingForDeps]),
            q: String::new(),
        }
        .to_string();
        assert!(queued.contains("s=enqueued,waiting"), "{queued}");

        let problems = Route::Logs {
            view: ViewParams::for_logs(Some(Severity::Warning), false, None),
        }
        .to_string();
        assert!(problems.contains("v=warning"), "{problems}");

        let longest = Route::Builds {
            view: ViewParams::with_sort(SortKey::Duration, SortDir::Desc),
            q: String::new(),
        }
        .to_string();
        assert!(longest.contains("o=duration-desc"), "{longest}");

        // Unfiltered cards link to the plain lists.
        let plain = Route::Packages {
            view: ViewParams::default(),
            q: String::new(),
        }
        .to_string();
        assert!(!plain.contains("s="), "{plain}");
    }
}
