//! One package: its state, what it depends on, and where it came from.
//!
//! Built around the question someone actually arrives with — "is this package
//! healthy, and if not what do I do" — rather than around the data model. The
//! header answers it; everything below is reference material.
//!
//! Deliberately not a list of builds, which would bury what matters: for each
//! architecture, the most recent build and — when that one failed — the older
//! build the repository still serves for it. The full history is a click away.

mod config;
mod relations;
mod removal;

use crate::api::client;
use crate::dates::RelativeDate;
use crate::format::{format_bytes, format_duration, now_secs};
use crate::routes::Route;
use crate::screens::package_scope::use_scoped_package;
use crate::status::BuildStatusBadge;
use crate::widgets::Spinner;
use aurcache_client::{Build, ExtendedPackage, PackageFile, PackageSource};
use aurcache_common::build_state::BuildState;
use config::BuildConfigCard;
use dioxus::prelude::*;
use relations::Relations;
use removal::{RemoveCard, ReplaceAndRemoveCard};

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

/// The package's most recent builds, for the summary.
async fn load_builds(pkgbase: String) -> Result<Vec<Build>, String> {
    client()?
        .list_builds(&aurcache_client::BuildQuery {
            pkgbase: Some(pkgbase),
            limit: Some(BUILD_SAMPLE),
            ..Default::default()
        })
        .await
        .map_err(|e| e.to_string())
}

#[component]
pub fn Package(pkgbase: String) -> Element {
    let mut package = use_scoped_package();
    let mut builds = use_resource(use_reactive(&pkgbase, load_builds));

    // Refresh while a recent build is still in flight, so one finishing
    // updates the summary without a reload; the scope does the same for the
    // package itself.
    let building = matches!(&*builds.read_unchecked(), Some(Ok(list))
        if list.iter().any(|b| b.status.is_in_progress()));
    crate::poll::use_poll(builds, building);

    let changed = move |()| {
        package.restart();
        builds.restart();
    };

    match &*package.read_unchecked() {
        Some(Ok(pkg)) => rsx! {
            // The sidebar starts level with the builds card rather than below
            // it, so the builds card is only as wide as it needs and the space
            // beside it is used.
            //
            // A fixed 26rem sidebar rather than a fraction: its content is URLs
            // and versions, which wrap badly, and a fraction of a wide viewport
            // is more than they need while a fraction of a narrow one is less.
            div { class: "grid grid-cols-1 lg:grid-cols-[minmax(0,1fr)_26rem] gap-4 items-start",
                // Dependency lists are unbounded — thirty entries is ordinary —
                // so they take the flexible column.
                div { class: "space-y-4 min-w-0",
                    // A failed build query does not lose the package itself:
                    // the summary is secondary, and the rest is still worth
                    // showing.
                    match &*builds.read_unchecked() {
                        None => rsx! { Spinner {} },
                        Some(Ok(list)) => rsx! { BuildSummary { pkgbase: pkg.name.clone(), builds: list.clone() } },
                        Some(Err(error)) => rsx! { div { class: "alert alert-error", "Could not load build history: {error}" } },
                    }
                    Relations { pkg: pkg.clone(), on_changed: changed }
                    crate::screens::logs::RecentActivity {
                        about: aurcache_client::PackageRef::from(pkg.name.as_str()).into(),
                    }
                    // Last in this column rather than full width under both:
                    // it is the one irreversible action here, and it has no
                    // business sitting beside Rebuild where people click
                    // without reading -- but spanning the page put it below a
                    // sidebar that had already run out, leaving the space
                    // beside it doing nothing.
                    //
                    // A package that is only here as a dependency cannot be
                    // removed by clearing a flag that is already clear -- its
                    // dependents have to stop needing it first. One that was
                    // asked for keeps the plain Remove, which clears the flag
                    // and leaves it as a dependency; removing it then is the
                    // second step, from this same card.
                    if !pkg.directly_requested && !pkg.dependents.is_empty() {
                        ReplaceAndRemoveCard {
                            pkgbase: pkg.name.clone(),
                            dependents: pkg.dependents.clone(),
                            on_changed: changed,
                        }
                    } else {
                        RemoveCard {
                            pkgbase: pkg.name.clone(),
                            dependents: pkg.dependents.len(),
                            on_changed: changed,
                        }
                    }
                }
                div { class: "space-y-4 min-w-0",
                    SourceCard { pkg: pkg.clone() }
                    BuildConfigCard { pkg: pkg.clone(), on_changed: changed }
                    ArtifactsCard { pkg: pkg.clone() }
                }
            }
        },
        // The scope above says why it failed, and a missing package is on its
        // way to the add page.
        Some(Err(_)) => rsx! {},
        None => rsx! { Spinner {} },
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

    // In display order: each architecture's most recent build, and after a
    // failed one the build the repository still serves for it.
    let mut rows: Vec<SummaryRow> = Vec::new();
    for platform in platforms_of(&builds) {
        let Some(newest) = latest_on(&builds, &platform) else {
            continue;
        };
        rows.push(SummaryRow {
            label: platform.clone(),
            build: newest.clone(),
        });
        // A second line only when that most recent build did not succeed:
        // otherwise it *is* what the repository serves, so `in_repo_on` would
        // return the same build and the row would just be noise.
        if !succeeded(newest)
            && let Some(repo) = in_repo_on(&builds, &platform)
        {
            rows.push(SummaryRow {
                label: "↳ in repo".to_string(),
                build: repo.clone(),
            });
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
                        for row in rows.iter() {
                            BuildRow {
                                // No index prefix: an insert above would
                                // restamp every key below it, which is what
                                // keys exist to survive. Platform and number
                                // are unique across a package's builds.
                                key: "{row.label}-{row.build.number}",
                                label: row.label.clone(),
                                entry: row.build.clone(),
                                now,
                            }
                        }
                    }
                }
            }
        }
    }
}

/// A line of the build summary before it is rendered.
struct SummaryRow {
    label: String,
    build: Build,
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
    fn SourceHarness(pkg: ExtendedPackage) -> Element {
        rsx! { SourceCard { pkg } }
    }

    fn render_source(pkg: &ExtendedPackage) -> String {
        let mut dom =
            VirtualDom::new_with_props(SourceHarness, SourceHarnessProps { pkg: pkg.clone() });
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    /// The AUR link is the header's: the Source card keeps everything else it
    /// knows from the AUR — here the flagged-out-of-date warning — but has no
    /// Origin row.
    #[test]
    fn the_source_card_leaves_the_aur_link_to_the_header() {
        let html = render_source(&aur_package());
        assert!(!html.contains("aur.archlinux.org"), "{html}");
        assert!(!html.contains("Origin"), "{html}");
        assert!(html.contains("Flagged out of date"), "{html}");
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
