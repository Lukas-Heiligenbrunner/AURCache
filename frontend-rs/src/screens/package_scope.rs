//! What every page under `/package/:pkgbase` shares: the package, fetched once
//! for all of them and kept fresh, the redirect when it is not here, and the
//! header naming it.

use crate::action::use_action;
use crate::api::{LoadError, client};
use crate::listing::ViewParams;
use crate::routes::Route;
use crate::status::StatusBadge;
use aurcache_client::{ExtendedPackage, PackageSource};
use dioxus::prelude::*;

/// The package a page is about, as [`PackageScope`] fetched it.
#[derive(Clone, Copy)]
struct ScopedPackage(Resource<Result<ExtendedPackage, LoadError>>);

/// The package the current page is about, for a page under [`PackageScope`].
///
/// Restarting it refreshes the header too: there is one fetch for both.
pub fn use_scoped_package() -> Resource<Result<ExtendedPackage, LoadError>> {
    use_context::<ScopedPackage>().0
}

/// The layout every package-scoped page renders in.
#[component]
pub fn PackageScope() -> Element {
    let route = use_route::<Route>();
    // Every route under this layout names a package.
    let pkgbase = route.pkgbase().unwrap_or_default().to_string();
    // `use_reactive` so the fetch follows the route. Navigating between two
    // packages reuses this layout -- same route, different parameter -- and a
    // resource whose closure captured the old name never re-runs: the URL
    // changes, no request is made, and the previous package stays on screen
    // looking like the one that was clicked.
    let mut package = use_resource(use_reactive(&pkgbase, |pkgbase| async move {
        Ok::<_, LoadError>(client()?.get_package(&pkgbase).await?)
    }));
    use_context_provider(|| ScopedPackage(package));

    // A package that is not here is one you might want to add: a stale link
    // (a log entry, a bookmark) lands on the add page with the name searched.
    let notice = crate::notice::use_notice();
    use_effect(use_reactive(&pkgbase, move |pkgbase| {
        if matches!(&*package.read(), Some(Err(LoadError::NotFound))) {
            crate::notice::redirect(
                notice,
                Route::PackageAdd { q: pkgbase.clone() },
                crate::notice::Level::Info,
                format!("No package called {pkgbase} is tracked here. Search the AUR to add it."),
            );
        }
    }));

    // Refresh briskly while one of its builds is under way, so the header's
    // status follows a build finishing without a reload.
    let busy = matches!(&*package.read_unchecked(), Some(Ok(pkg)) if pkg.status.is_in_progress());
    crate::poll::use_poll(package, busy);

    rsx! {
        div { class: if fills_window(&route) { "space-y-4 h-full flex flex-col min-h-0" } else { "space-y-4" },
            match &*package.read_unchecked() {
                Some(Ok(pkg)) => rsx! {
                    PackageHeader {
                        pkg: pkg.clone(),
                        trail: trail(&route),
                        on_rebuilt: move |()| package.restart(),
                    }
                },
                // The page below may still have something to show: an editor
                // needs only the name, which came from the URL.
                Some(Err(LoadError::Failed(e))) => rsx! {
                    div { class: "alert alert-error", span { "Could not load {pkgbase}: {e}" } }
                },
                // Still loading, or on its way to the add page.
                _ => rsx! {},
            }
            Outlet::<Route> {}
        }
    }
}

/// Whether the page fills the window and scrolls a pane of its own, rather than
/// running long: the build log does, and needs a column with a height to flex
/// into.
fn fills_window(route: &Route) -> bool {
    matches!(route, Route::Build { .. })
}

/// One step of the header's trail after the package: where it leads, if it is
/// not the page you are on.
#[derive(Clone, PartialEq, Debug)]
pub struct Crumb {
    pub label: String,
    pub to: Option<Route>,
}

/// The trail after the package's name, for the page `route` shows.
fn trail(route: &Route) -> Vec<Crumb> {
    let here = |label: &str| Crumb {
        label: label.to_string(),
        to: None,
    };
    match route {
        Route::PackageBuilds { .. } => vec![here("Builds")],
        Route::Build { pkgbase, number } => vec![
            Crumb {
                label: "Builds".to_string(),
                to: Some(Route::PackageBuilds {
                    pkgbase: pkgbase.clone(),
                }),
            },
            here(&number.to_string()),
        ],
        Route::PackageConfigFiles { .. } => vec![here("Config files")],
        Route::PackageSource { .. } => vec![here("Sources")],
        _ => vec![],
    }
}

#[component]
pub fn PackageHeader(
    pkg: ExtendedPackage,
    trail: Vec<Crumb>,
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
                                    for (index, crumb) in trail.iter().enumerate() {
                                        span { key: "sep-{index}", class: "opacity-30 mx-2", "/" }
                                        match crumb.to.clone() {
                                            Some(route) => rsx! {
                                                Link {
                                                    key: "{index}",
                                                    class: "opacity-60 link-hover",
                                                    to: route,
                                                    "{crumb.label}"
                                                }
                                            },
                                            None => rsx! { span { key: "{index}", "{crumb.label}" } },
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

/// Queue a rebuild of this package and land on the new build's page.
///
/// Disabled while in flight so a double click does not queue twice, and it
/// reports what happened rather than silently doing nothing. `update_package`
/// returns the build numbers it queued — one per platform — so the result is
/// shown rather than waited for: the destination build's page is the update.
#[component]
fn RebuildButton(pkgbase: String, on_changed: EventHandler<()>) -> Element {
    let action = use_action();

    rsx! {
        div { class: "flex items-center gap-2",
            if let Some(message) = action.error() {
                span { class: "text-xs text-error", "{message}" }
            }
            button {
                class: "btn btn-primary btn-sm",
                disabled: action.busy(),
                onclick: move |_| {
                    let pkgbase = pkgbase.clone();
                    async move {
                        let queued = action
                            .run(async {
                                client()?
                                    .update_package(&pkgbase, &aurcache_client::UpdatePackage { force: true })
                                    .await
                                    .map_err(|e| e.to_string())
                            })
                            .await;
                        if let Some(numbers) = queued {
                            on_changed.call(());
                            if let Some(number) = numbers.into_iter().next() {
                                navigator().push(Route::Build { pkgbase, number });
                            }
                        }
                    }
                },
                if action.busy() {
                    span { class: "loading loading-spinner loading-xs" }
                }
                "Rebuild"
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aurcache_common::build_state::BuildState;

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

    /// The header is the one place every package-scoped page shares, so it
    /// carries the AUR link -- the builds list and the build page included.
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

    /// Every package-scoped page offers a rebuild, the build page included.
    #[test]
    fn the_rebuild_button_sits_in_the_header() {
        for pkg in [package(), aur_package()] {
            assert!(render_header(&pkg).contains("Rebuild"), "{pkg:?}");
        }
    }

    /// Each page under the package says where it is after the package's name;
    /// a build's trail leads back to the builds it is one of.
    #[test]
    fn each_page_has_its_own_trail() {
        let pkgbase = || "hello".to_string();
        assert!(trail(&Route::Package { pkgbase: pkgbase() }).is_empty());
        assert_eq!(
            trail(&Route::PackageBuilds { pkgbase: pkgbase() }),
            vec![Crumb {
                label: "Builds".to_string(),
                to: None,
            }]
        );
        assert_eq!(
            trail(&Route::Build {
                pkgbase: pkgbase(),
                number: 3,
            }),
            vec![
                Crumb {
                    label: "Builds".to_string(),
                    to: Some(Route::PackageBuilds { pkgbase: pkgbase() }),
                },
                Crumb {
                    label: "3".to_string(),
                    to: None,
                },
            ]
        );
        assert_eq!(
            trail(&Route::PackageSource {
                pkgbase: pkgbase(),
                path: vec![],
            })[0]
                .label,
            "Sources"
        );
    }
}
