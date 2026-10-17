//! One package's `makepkg.conf` and `pacman.conf`.
//!
//! A page of its own because two full-height editors do not belong on a summary
//! page, and named for what it holds rather than for a category: it was
//! "settings" while it also carried platforms, flags and removal, and those
//! read better beside the values they change, on the package page.
//!
//! Everything here is an override. The package either holds its own copy of a
//! file or inherits the server-wide one, which is why each editor says which of
//! the two is in force.

use crate::routes::Route;
use crate::screens::PackageHeader;
use crate::screens::config_files::ConfigFileTabs;
use dioxus::prelude::*;

/// The screen behind `/package/:pkgbase/config-files`.
#[component]
pub fn PackageConfigFiles(pkgbase: String) -> Element {
    // `use_reactive` so the fetch follows the route. Navigating between two
    // packages reuses this component -- same route, different parameter -- and
    // a resource whose closure captured the old name simply never re-runs: the
    // URL changes, no request is made, and the previous package stays on
    // screen looking like the one that was clicked.
    let mut package = use_resource(use_reactive(&pkgbase, |pkgbase| async move {
        crate::api::client()?
            .get_package(&pkgbase)
            .await
            .map_err(|e| e.to_string())
    }));

    rsx! {
        div { class: "space-y-4",
            // The package's own header, so editing its files happens in sight
            // of what is being edited. Its absence must not block the editors:
            // they need only the name, which came from the URL.
            match &*package.read_unchecked() {
                Some(Ok(pkg)) => rsx! {
                    PackageHeader {
                        pkg: pkg.clone(),
                        trail: vec![("Config files".to_string(), None)],
                        on_rebuilt: move |()| package.restart(),
                    }
                },
                Some(Err(e)) => rsx! {
                    div { class: "alert alert-error", span { "Could not load {pkgbase}: {e}" } }
                },
                None => rsx! {},
            }

            div { class: "card bg-base-100 shadow-xl",
                div { class: "card-body",
                    p { class: "text-xs opacity-60 max-w-prose",
                        "Files this package builds against. Left unset, each inherits the "
                        Link { class: "link link-primary", to: Route::ConfigFiles {},
                            "server-wide file"
                        }
                        "."
                    }
                    ConfigFileTabs { pkgbase: Some(pkgbase) }
                }
            }
        }
    }
}
