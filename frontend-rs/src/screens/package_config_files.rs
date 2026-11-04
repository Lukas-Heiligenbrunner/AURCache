//! One package's `makepkg.conf` and `pacman.conf`.
//!
//! A page of its own because two full-height editors do not belong on a summary
//! page, and named for what it holds rather than for a category.
//!
//! Everything here is an override. The package either holds its own copy of a
//! file or inherits the server-wide one, which is why each editor says which of
//! the two is in force.

use crate::routes::Route;
use crate::screens::config_files::ConfigFileTabs;
use dioxus::prelude::*;

/// The screen behind `/package/:pkgbase/config-files`.
///
/// The editors need only the name, which came from the URL, so they do not
/// wait for the package the header above shows.
#[component]
pub fn PackageConfigFiles(pkgbase: String) -> Element {
    rsx! {
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
