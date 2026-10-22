//! Which architectures a package is built for.
//!
//! Shared because the same list is picked twice — once when a package is added
//! and again when it is edited — and two copies would drift the moment an
//! architecture is added.

use dioxus::prelude::*;

/// The architectures a build can target.
///
/// Whether a given package *can* build for one of these depends entirely on its
/// PKGBUILD, which is why the picker says so rather than pretending to know.
pub const ALL: [&str; 3] = ["x86_64", "aarch64", "armv7h"];

/// The default for a new package: what almost every PKGBUILD supports, and the
/// only one that needs no emulation.
pub const DEFAULT: &str = "x86_64";

/// The selection after ticking or unticking one architecture.
///
/// Order is preserved and a repeated tick is a no-op, so a set cannot grow a
/// duplicate — the same platform twice would be sent to the server twice.
pub fn toggled(selected: &[String], platform: &str, checked: bool) -> Vec<String> {
    let mut next: Vec<String> = selected.to_vec();
    if checked {
        if !next.iter().any(|p| p == platform) {
            next.push(platform.to_string());
        }
    } else {
        next.retain(|p| p != platform);
    }
    next
}

/// A checkbox per architecture, reporting the whole selection on every change.
///
/// Reports the set rather than the toggle so callers hold one value; a caller
/// that had to apply add/remove events would be re-implementing this list.
#[component]
pub fn PlatformChecklist(
    selected: Vec<String>,
    onchange: EventHandler<Vec<String>>,
    #[props(default = String::from("checkbox-xs"))] size: String,
    #[props(default = false)] disabled: bool,
) -> Element {
    rsx! {
        div { class: "flex flex-col gap-1",
            for platform in ALL {
                label { key: "{platform}", class: "flex items-center gap-2 cursor-pointer",
                    input {
                        r#type: "checkbox",
                        class: "checkbox {size}",
                        disabled,
                        checked: selected.iter().any(|p| p == platform),
                        onchange: {
                            let selected = selected.clone();
                            move |e: FormEvent| {
                                onchange.call(toggled(&selected, platform, e.checked()));
                            }
                        },
                    }
                    span { class: "font-mono text-xs", "{platform}" }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ALL, DEFAULT, toggled};

    /// The default has to be one of the offered architectures, or a new package
    /// starts out asking for something the picker cannot show.
    #[test]
    fn the_default_platform_is_one_of_the_offered_ones() {
        assert!(ALL.contains(&DEFAULT));
    }

    fn set(items: &[&str]) -> Vec<String> {
        items.iter().map(ToString::to_string).collect()
    }

    /// Ticking reports the whole set, not the box that moved. Reporting only
    /// the toggled one would quietly narrow a two-platform build to one.
    #[test]
    fn ticking_a_box_adds_it_to_the_set() {
        assert_eq!(
            toggled(&set(&["x86_64"]), "aarch64", true),
            set(&["x86_64", "aarch64"])
        );
    }

    #[test]
    fn unticking_a_box_removes_only_that_one() {
        assert_eq!(
            toggled(&set(&["x86_64", "aarch64"]), "x86_64", false),
            set(&["aarch64"])
        );
    }

    /// The same platform twice would be sent to the server twice.
    #[test]
    fn ticking_an_already_selected_box_changes_nothing() {
        assert_eq!(toggled(&set(&["x86_64"]), "x86_64", true), set(&["x86_64"]));
    }

    /// Unticking something absent is not an error, just nothing.
    #[test]
    fn unticking_an_unselected_box_changes_nothing() {
        assert_eq!(
            toggled(&set(&["x86_64"]), "aarch64", false),
            set(&["x86_64"])
        );
    }
}
