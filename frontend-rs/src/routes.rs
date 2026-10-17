//! The route table.
//!
//! Paths are stable across rewrites so existing links keep working. Each
//! variant names a component of the same name, and the fields are that
//! component's props — a route that does not line up with its screen is a
//! compile error rather than a blank page.

// `unreachable_code` fires inside the `Routable` expansion rather than on
// anything written here, and the derive generates an `impl` an attribute on the
// enum does not reach -- so the allow has to sit on the module. It started
// warning with the 2026-08-28 nightly; the alternative is failing every build
// on a lint about code we do not write.
//
// The scope is this file, which is the route table, one match over it and its
// tests. None of that has any business containing unreachable code, so the
// suppression is unlikely to hide anything of ours.
#![allow(unreachable_code)]

use crate::listing::ViewParams;
use crate::screens::*;
use crate::shell::MenuShell;
use dioxus::prelude::*;

#[derive(Routable, Clone, PartialEq, Debug)]
#[rustfmt::skip]
pub enum Route {
    // Everything inside the shell renders in the drawer's content area, so the
    // side menu is mounted once rather than per screen.
    #[layout(MenuShell)]
        #[route("/")]
        Dashboard {},

        // `#:q` carries the filter. In the fragment rather than a query string
        // because dioxus's query handling has two defects a search term walks
        // straight into: an empty value still writes `?q=`, so every unfiltered
        // URL grows a dangling marker, and the value is not escaped on write
        // while being split on `&` on read, so a term containing one is
        // truncated. A fragment writes nothing when empty and is one opaque
        // string. Nothing here is server-rendered and the filtering is
        // client-side, so keeping the term out of the request costs nothing.
        // Filter and sort ride in the query as one spread segment, so their
        // encoding belongs to `ViewParams` rather than to the router. A *named*
        // parameter cannot be used here: dioxus writes `name=` whether or not
        // there is a value, so `?:status` alone hung a `?status=` on every
        // unfiltered URL. See `ViewParams` for the one `?` that still escapes.
        #[route("/builds?:..view#:q")]
        Builds { view: ViewParams, q: String },


        #[route("/packages?:..view#:q")]
        Packages { view: ViewParams, q: String },
        // A dialog over the list rather than a page, but with a URL of its own
        // so it can be linked to and Back closes it.
        #[route("/packages/add#:q")]
        PackageAdd { q: String },
        #[route("/package/:pkgbase")]
        Package { pkgbase: String },
        #[route("/package/:pkgbase/builds")]
        PackageBuilds { pkgbase: String },
        #[route("/package/:pkgbase/build/:number")]
        Build { pkgbase: String, number: i32 },
        // Named for what it holds. It was "settings" while it also carried
        // platforms, flags and removal; those live on the package page now,
        // and two config files are not a settings page.
        #[route("/package/:pkgbase/config-files")]
        PackageConfigFiles { pkgbase: String },
        // `:..path` is a catch-all: source paths contain slashes, so a single
        // segment would only ever match files at the top level.
        #[route("/package/:pkgbase/source/:..path")]
        PackageSource { pkgbase: String, path: Vec<String> },

        #[route("/workers")]
        Workers {},
        // Singular for one of them, as `/package/:pkgbase` is beside
        // `/packages`. By name, which is what an operator has in hand -- but a
        // name is whatever a machine reports and a retired row keeps its own
        // for ever, so two workers can share one, and the name alone then
        // offers a choice rather than guessing.
        //
        // `:..name` is a catch-all because a worker name is free text: nothing
        // stops `WORKER_NAME` containing a slash, and a single segment would
        // silently resolve `/worker/ci/runner` as something else.
        #[route("/worker/:..name")]
        Worker { name: Vec<String> },
        // The escape hatch out of a shared name, and the one URL here that does
        // not carry one: the fingerprint is the identity, so it needs no help
        // to pick a worker out. Under `/workers/` rather than `/worker/` so it
        // cannot be mistaken for a name by the catch-all above.
        #[route("/workers/by-cert/:fingerprint")]
        WorkerByFingerprint { fingerprint: String },
        // Renamed from Activities once it carried failures as well as actions.
        // The old path still lands: a bookmark predates the rename.
        #[redirect("/activities", || Route::Logs { view: ViewParams::default() })]
        #[route("/logs?:..view")]
        Logs { view: ViewParams },
        #[route("/settings")]
        Settings {},
        // Under Settings because that is the only way in: it left the sidebar
        // once the section it belongs to was the one place that linked it.
        #[redirect("/config-files", || Route::ConfigFiles {})]
        #[route("/settings/config-files")]
        ConfigFiles {},

        #[route("/:..segments")]
        NotFound { segments: Vec<String> },
}

/// A top-level entry in the side menu.
///
/// Separate from [`Route`] because several routes belong to one entry: viewing
/// a single build keeps "Builds" highlighted, so entries match on a path
/// prefix rather than an exact route.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MenuEntry {
    Dashboard,
    Builds,
    Packages,
    Logs,
    Workers,
    Settings,
}

impl Route {
    /// Which menu entry should be highlighted while this route is open.
    ///
    /// `None` for routes with no home in the menu, so nothing is highlighted
    /// rather than something arbitrary.
    pub fn menu_entry(&self) -> Option<MenuEntry> {
        match self {
            Self::Dashboard { .. } => Some(MenuEntry::Dashboard),
            Self::Builds { .. } => Some(MenuEntry::Builds),
            // A single build lives under its package now — same URL, same
            // breadcrumb, same header — so it highlights Packages with the
            // rest of them rather than jumping the menu to Builds.
            Self::Packages { .. }
            | Self::PackageAdd { .. }
            | Self::Package { .. }
            | Self::PackageBuilds { .. }
            | Self::Build { .. }
            | Self::PackageSource { .. }
            | Self::PackageConfigFiles { .. } => Some(MenuEntry::Packages),
            Self::Logs { .. } => Some(MenuEntry::Logs),
            Self::Workers { .. } | Self::Worker { .. } | Self::WorkerByFingerprint { .. } => {
                Some(MenuEntry::Workers)
            }
            // Reached from the Settings page and nowhere else, so it keeps
            // that entry highlighted rather than clearing the menu.
            Self::Settings { .. } | Self::ConfigFiles { .. } => Some(MenuEntry::Settings),
            Self::NotFound { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aurcache_client::Severity;
    use aurcache_common::build_state::BuildState;
    use std::str::FromStr;

    fn entry_for(path: &str) -> Option<MenuEntry> {
        Route::from_str(path)
            .unwrap_or_else(|e| panic!("{path} should parse: {e}"))
            .menu_entry()
    }

    /// The whole point of the menu entry being derived from the route: a
    /// sub-page keeps its section highlighted instead of clearing the menu.
    #[test]
    fn sub_pages_highlight_their_section() {
        assert_eq!(entry_for("/builds"), Some(MenuEntry::Builds));

        assert_eq!(entry_for("/packages"), Some(MenuEntry::Packages));
        assert_eq!(entry_for("/package/hello"), Some(MenuEntry::Packages));
        assert_eq!(
            entry_for("/package/hello/builds"),
            Some(MenuEntry::Packages)
        );
        assert_eq!(
            entry_for("/package/hello/build/3"),
            Some(MenuEntry::Packages)
        );
        assert_eq!(
            entry_for("/package/hello/source/PKGBUILD"),
            Some(MenuEntry::Packages)
        );
        assert_eq!(
            entry_for("/package/hello/config-files"),
            Some(MenuEntry::Packages)
        );

        assert_eq!(entry_for("/workers"), Some(MenuEntry::Workers));
        assert_eq!(entry_for("/worker/builder-01"), Some(MenuEntry::Workers));
        assert_eq!(
            entry_for("/workers/by-cert/0f1e2d3c4b5a"),
            Some(MenuEntry::Workers)
        );
    }

    /// A page with no search must not carry a marker for one. This is the
    /// concrete reason the term is in the fragment: dioxus writes `?q=` even
    /// for an empty value, so every unfiltered URL would have grown one.
    ///
    /// The lists now spread their filter and sort into the query, and dioxus
    /// emits the `?` before consulting the type, so an unfiltered list ends in
    /// a bare `?` (DioxusLabs/dioxus#5792, fixed by the open #5793). That much
    /// is tolerated; anything *after* it is not, which is what keeps
    /// `ViewParams` honest about writing nothing for a default view.
    #[test]
    fn an_empty_search_leaves_no_trace_in_the_url() {
        for url in [
            Route::Packages {
                view: ViewParams::default(),
                q: String::new(),
            }
            .to_string(),
            Route::Builds {
                view: ViewParams::default(),
                q: String::new(),
            }
            .to_string(),
        ] {
            let query = url.split_once('?').map_or("", |(_, q)| q);
            assert!(
                query.is_empty(),
                "a default view wrote query {query:?} into {url:?}"
            );
        }
        // No query segment on this route, so not even the `?` is allowed.
        assert_eq!(
            Route::PackageAdd { q: String::new() }.to_string(),
            "/packages/add"
        );
    }

    /// A URL with no fragment is the same page as one with an empty search,
    /// which is what makes a plain `/packages` link work.
    #[test]
    fn a_url_without_a_fragment_is_an_empty_search() {
        assert_eq!(
            Route::from_str("/packages").unwrap(),
            Route::Packages {
                view: ViewParams::default(),
                q: String::new()
            }
        );
    }

    /// Activity became Logs when it started carrying failures as well as
    /// actions. A bookmark of the old path still has to land on them.
    #[test]
    fn the_old_activities_path_still_arrives() {
        assert_eq!(entry_for("/activities"), Some(MenuEntry::Logs));
        assert_eq!(entry_for("/logs"), Some(MenuEntry::Logs));
    }

    /// An unfiltered log carries no filter in its URL, and a filtered one
    /// carries exactly what was set.
    #[test]
    fn log_filters_ride_the_query() {
        let bare = Route::Logs {
            view: ViewParams::default(),
        }
        .to_string();
        assert!(
            bare.split_once('?').is_none_or(|(_, q)| q.is_empty()),
            "an unfiltered log wrote {bare:?}"
        );

        let filtered = Route::Logs {
            view: ViewParams::for_logs(Some(Severity::Warning), true, None),
        }
        .to_string();
        assert!(filtered.contains("v=warning"), "{filtered}");
        assert!(filtered.contains("b=1"), "{filtered}");
    }

    /// The config files moved under Settings when they left the sidebar. A
    /// bookmark of the old path still has to land on them.
    #[test]
    fn the_old_config_files_path_still_arrives() {
        assert_eq!(Route::ConfigFiles {}.to_string(), "/settings/config-files");
        assert_eq!(entry_for("/config-files"), Some(MenuEntry::Settings));
        assert_eq!(
            entry_for("/settings/config-files"),
            Some(MenuEntry::Settings)
        );
    }

    /// `/` is the landing route, and hash routing depends on it: an empty
    /// fragment is turned into `/`, so this is what a first visit renders.
    #[test]
    fn the_index_route_is_the_dashboard() {
        assert_eq!(entry_for("/"), Some(MenuEntry::Dashboard));
    }

    /// Nothing in the menu corresponds to a bad URL, so nothing is highlighted.
    #[test]
    fn an_unknown_path_highlights_nothing() {
        assert_eq!(entry_for("/no/such/page"), None);
    }

    /// Round-trips through the string form the URL actually carries. Catches a
    /// route whose `Display` and parser disagree, which would make a `Link`
    /// navigate somewhere it cannot parse back.
    #[test]
    fn routes_round_trip_through_their_url() {
        for route in [
            Route::Dashboard {},
            Route::Builds {
                view: ViewParams::default(),
                q: String::new(),
            },
            // Filter and sort round-trip through the query, beside a term in
            // the fragment.
            Route::Builds {
                view: ViewParams::with_status(BuildState::Active),
                q: "freyja".to_string(),
            },
            // A search term goes in the fragment, and these are the shapes that
            // broke it as a query parameter: `&` split the value in two, and an
            // empty one still wrote a marker into every unfiltered URL.
            Route::Builds {
                view: ViewParams::default(),
                q: "a&b".to_string(),
            },
            Route::Packages {
                view: ViewParams::default(),
                q: "aewm++".to_string(),
            },
            Route::PackageAdd {
                q: "two words".to_string(),
            },
            Route::Build {
                pkgbase: "hello".into(),
                number: 42,
            },
            Route::Packages {
                view: ViewParams::default(),
                q: String::new(),
            },
            Route::Package {
                pkgbase: "hello".into(),
            },
            Route::PackageSource {
                pkgbase: "hello".into(),
                path: vec!["PKGBUILD".into()],
            },
            Route::PackageConfigFiles {
                pkgbase: "hello".into(),
            },
            // A nested source file: the catch-all segment has to survive both
            // directions, or deep links into subdirectories break.
            Route::PackageSource {
                pkgbase: "hello".into(),
                path: vec!["subdir".into(), "patch.diff".into()],
            },
            Route::Workers {},
            Route::Worker {
                name: vec!["builder-01".into()],
            },
            Route::WorkerByFingerprint {
                fingerprint: "0f1e2d3c4b5a".into(),
            },
            Route::Logs {
                view: ViewParams::default(),
            },
            Route::Logs {
                view: ViewParams::for_logs(Some(Severity::Warning), true, None),
            },
            Route::Settings {},
            Route::ConfigFiles {},
        ] {
            let url = route.to_string();
            let parsed =
                Route::from_str(&url).unwrap_or_else(|e| panic!("{url} should parse back: {e}"));
            assert_eq!(parsed, route, "round trip through {url}");
        }
    }

    /// A worker is called whatever its machine calls itself, which is a
    /// hostname at best and arbitrary at worst -- so the name has to survive a
    /// URL the same way a pkgbase does.
    #[test]
    fn worker_names_with_url_significant_characters_survive() {
        for name in [
            "builder-01",
            "build.example.com",
            "worker 2",
            "a+b",
            "ci/runner",
        ] {
            let route = Route::Worker {
                name: name.split('/').map(str::to_string).collect(),
            };
            let url = route.to_string();
            assert_eq!(
                Route::from_str(&url).ok(),
                Some(route),
                "worker name {name} broke on the way through {url}"
            );
        }
    }

    /// A package name with characters that are legal in pkgbase but meaningful
    /// in a URL. `+` is the one that bit the HTTP layer before.
    #[test]
    fn package_names_with_url_significant_characters_survive() {
        for name in ["gtk3+", "lib32-glibc", "python-3.11", "1337"] {
            let route = Route::Package {
                pkgbase: name.to_string(),
            };
            let url = route.to_string();
            assert_eq!(
                Route::from_str(&url).ok(),
                Some(route),
                "pkgbase {name} broke on the way through {url}"
            );
        }
    }

    /// The package screen links to the editor with no file selected, so the
    /// empty catch-all has to survive the trip too.
    #[test]
    fn the_source_editor_link_works_with_no_file_selected() {
        let route = Route::PackageSource {
            pkgbase: "hello".into(),
            path: vec![],
        };
        let url = route.to_string();
        assert_eq!(
            Route::from_str(&url).ok(),
            Some(route),
            "empty source path did not round trip through {url}"
        );
    }
}
