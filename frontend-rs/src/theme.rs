//! Theme selection, stored in the browser.
//!
//! Deliberately *not* a server setting. `ApplicationSettings` is scoped per
//! package or globally, with no per-user scope — so a theme stored there would
//! be shared by everyone, and one person switching to light would flip it for
//! every other user. It is also a display preference that reasonably differs
//! between a laptop and a desktop, which is per-device rather than per-account.
//!
//! `localStorage` gives all of that for free, with no API, no migration, and no
//! round-trip before the first paint.

use dioxus::prelude::*;

/// Where the choice is stored. `index.html` reads the same key before the wasm
/// loads; see [`resolve`] for why that matters.
pub const STORAGE_KEY: &str = "aurcache-theme";

/// The value meaning "follow the operating system".
pub const SYSTEM: &str = "system";

/// What `system` resolves to. daisyUI names its two defaults exactly this.
const SYSTEM_DARK: &str = "dark";
const SYSTEM_LIGHT: &str = "light";

pub struct ThemeOption {
    pub id: &'static str,
    pub label: &'static str,
    /// Whether the palette is dark, used to group the picker.
    pub dark: bool,
}

/// A curated set rather than all 32 daisyUI themes: enough to pick a palette
/// you like without a menu nobody reads to the bottom of.
pub const THEMES: &[ThemeOption] = &[
    ThemeOption {
        id: "dark",
        label: "Dark",
        dark: true,
    },
    ThemeOption {
        id: "night",
        label: "Night",
        dark: true,
    },
    ThemeOption {
        id: "dim",
        label: "Dim",
        dark: true,
    },
    ThemeOption {
        id: "sunset",
        label: "Sunset",
        dark: true,
    },
    ThemeOption {
        id: "dracula",
        label: "Dracula",
        dark: true,
    },
    ThemeOption {
        id: "forest",
        label: "Forest",
        dark: true,
    },
    ThemeOption {
        id: "business",
        label: "Business",
        dark: true,
    },
    ThemeOption {
        id: "coffee",
        label: "Coffee",
        dark: true,
    },
    ThemeOption {
        id: "synthwave",
        label: "Synthwave",
        dark: true,
    },
    ThemeOption {
        id: "light",
        label: "Light",
        dark: false,
    },
    ThemeOption {
        id: "corporate",
        label: "Corporate",
        dark: false,
    },
    ThemeOption {
        id: "winter",
        label: "Winter",
        dark: false,
    },
    ThemeOption {
        id: "nord",
        label: "Nord",
        dark: false,
    },
    ThemeOption {
        id: "emerald",
        label: "Emerald",
        dark: false,
    },
    ThemeOption {
        id: "cupcake",
        label: "Cupcake",
        dark: false,
    },
    ThemeOption {
        id: "lofi",
        label: "Lo-fi",
        dark: false,
    },
    ThemeOption {
        id: "retro",
        label: "Retro",
        dark: false,
    },
];

/// A stored value, reduced to something safe to act on.
///
/// Anything unrecognised — a theme dropped from the list, or a hand-edited
/// `localStorage` — falls back to following the system rather than to a
/// `data-theme` daisyUI has no rules for, which renders unstyled.
pub fn normalise(stored: Option<&str>) -> &str {
    match stored {
        Some(value) if value == SYSTEM => SYSTEM,
        Some(value) => THEMES
            .iter()
            .find(|t| t.id == value)
            .map_or(SYSTEM, |t| t.id),
        None => SYSTEM,
    }
}

/// The daisyUI theme name to put on `data-theme`.
///
/// The same rule runs twice: here, and as a few lines of JavaScript in
/// `index.html` that apply the stored theme before the wasm loads. Without that
/// pre-boot pass the page paints the default theme first and visibly flips once
/// the app mounts. Keep the two in step.
pub fn resolve(choice: &str, prefers_dark: bool) -> &str {
    if choice != SYSTEM {
        return choice;
    }
    if prefers_dark {
        SYSTEM_DARK
    } else {
        SYSTEM_LIGHT
    }
}

// ---------------------------------------------------------------------------
// Browser plumbing
// ---------------------------------------------------------------------------

fn stored_choice() -> String {
    web_sys::window()
        .and_then(|w| w.local_storage().ok().flatten())
        .and_then(|s| s.get_item(STORAGE_KEY).ok().flatten())
        .map_or_else(|| SYSTEM.to_string(), |v| normalise(Some(&v)).to_string())
}

fn prefers_dark() -> bool {
    web_sys::window()
        .and_then(|w| w.match_media("(prefers-color-scheme: dark)").ok().flatten())
        .is_some_and(|m| m.matches())
}

/// Write the choice and apply it to the document.
fn apply(choice: &str) {
    let Some(window) = web_sys::window() else {
        return;
    };
    if let Ok(Some(storage)) = window.local_storage() {
        let _ = storage.set_item(STORAGE_KEY, choice);
    }
    if let Some(root) = window.document().and_then(|d| d.document_element()) {
        let _ = root.set_attribute("data-theme", resolve(choice, prefers_dark()));
    }
}

/// A `<select>` of themes, grouped into dark and light.
///
/// A plain select rather than a custom dropdown: it is keyboard accessible and
/// gets the platform's native picker on a phone for free.
#[component]
pub fn ThemePicker() -> Element {
    let mut choice = use_signal(stored_choice);

    rsx! {
        label { class: "flex flex-col gap-1 py-2",
            span { class: "text-xs opacity-60", "Theme" }
            select {
                class: "select select-bordered select-sm w-full",
                value: "{choice}",
                onchange: move |e| {
                    let picked = e.value();
                    apply(&picked);
                    choice.set(picked);
                },
                option {
                    value: SYSTEM,
                    selected: choice() == SYSTEM,
                    "Follow system"
                }
                optgroup { label: "Dark",
                    for theme in THEMES.iter().filter(|t| t.dark) {
                        option {
                            key: "{theme.id}",
                            value: theme.id,
                            selected: choice() == theme.id,
                            "{theme.label}"
                        }
                    }
                }
                optgroup { label: "Light",
                    for theme in THEMES.iter().filter(|t| !t.dark) {
                        option {
                            key: "{theme.id}",
                            value: theme.id,
                            selected: choice() == theme.id,
                            "{theme.label}"
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn following_the_system_picks_a_palette_from_the_os() {
        assert_eq!(resolve(SYSTEM, true), "dark");
        assert_eq!(resolve(SYSTEM, false), "light");
    }

    /// An explicit choice is not overridden by the OS preference — picking a
    /// light theme on a dark-mode machine has to stick.
    #[test]
    fn an_explicit_theme_ignores_the_system_preference() {
        assert_eq!(resolve("dracula", false), "dracula");
        assert_eq!(resolve("cupcake", true), "cupcake");
    }

    /// daisyUI styles `[data-theme=x]` for the themes it ships. An unknown
    /// value matches no rule and renders the app unstyled, so a stale or
    /// hand-edited value has to fall back rather than be passed through.
    #[test]
    fn an_unknown_stored_value_falls_back_to_the_system() {
        assert_eq!(normalise(Some("no-such-theme")), SYSTEM);
        assert_eq!(normalise(Some("")), SYSTEM);
        assert_eq!(normalise(None), SYSTEM);
    }

    #[test]
    fn a_known_theme_survives_normalisation() {
        assert_eq!(normalise(Some("nord")), "nord");
        assert_eq!(normalise(Some(SYSTEM)), SYSTEM);
    }

    /// Duplicates would render twice in the picker, and an id colliding with
    /// `system` would make "follow the OS" unselectable.
    #[test]
    fn theme_ids_are_unique_and_none_shadows_system() {
        let mut seen = std::collections::HashSet::new();
        for theme in THEMES {
            assert!(theme.id != SYSTEM, "{} shadows the system option", theme.id);
            assert!(!theme.id.is_empty() && !theme.label.is_empty());
            assert!(seen.insert(theme.id), "duplicate theme id {}", theme.id);
        }
    }

    /// Both groups must be non-empty or the picker renders an empty optgroup.
    #[test]
    fn the_picker_offers_both_dark_and_light_palettes() {
        assert!(THEMES.iter().any(|t| t.dark));
        assert!(THEMES.iter().any(|t| !t.dark));
    }
}
