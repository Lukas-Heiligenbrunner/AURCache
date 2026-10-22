//! How timestamps are written.
//!
//! Two separate decisions, deliberately not conflated:
//!
//! - **Relative or absolute** is the *call site's* choice, not the viewer's. A
//!   package page answers "is this stale", so it wants "3h ago"; a build list
//!   is a log, so it wants a date. A preference cannot know which.
//! - **How an absolute date reads** is the viewer's choice: field order,
//!   leading zeros, and a 12- or 24-hour clock.
//!
//! Where a site shows relative time it carries the absolute date as hover
//! text, so "3h ago" never hides exactly when.
//!
//! Everything here is numeric. That is what the disagreement is actually about
//! — `2026-08-26` against `26/08/2026` against `08/26/2026` — and it sidesteps
//! month names, which would otherwise be English hardcoded into a UI that is
//! not translated yet.
//!
//! The server supplies the default; a browser overrides it locally. Nothing
//! writes a per-browser choice back to the server.

use dioxus::prelude::*;

pub const STORAGE_KEY: &str = "aurcache-date-format";

/// The order the date fields appear in, and the separator that goes with it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DateOrder {
    /// `2026-08-26`
    Ymd,
    /// `26/08/2026`
    Dmy,
    /// `08/26/2026`
    Mdy,
}

impl DateOrder {
    pub const ALL: [Self; 3] = [Self::Ymd, Self::Dmy, Self::Mdy];

    pub fn id(self) -> &'static str {
        match self {
            Self::Ymd => "ymd",
            Self::Dmy => "dmy",
            Self::Mdy => "mdy",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Ymd => "YYYY-MM-DD",
            Self::Dmy => "DD/MM/YYYY",
            Self::Mdy => "MM/DD/YYYY",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Clock {
    H24,
    H12,
}

impl Clock {
    pub const ALL: [Self; 2] = [Self::H24, Self::H12];

    pub fn id(self) -> &'static str {
        match self {
            Self::H24 => "24",
            Self::H12 => "12",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::H24 => "24-hour (14:05)",
            Self::H12 => "12-hour (2:05 pm)",
        }
    }
}

/// How absolute timestamps are written.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DateStyle {
    pub order: DateOrder,
    /// Zero-pad day and month — `08/26` against `8/26`.
    pub pad: bool,
    pub clock: Clock,
}

impl Default for DateStyle {
    /// ISO order, padded, 24-hour: unambiguous to everyone, whatever they are
    /// used to reading.
    fn default() -> Self {
        Self {
            order: DateOrder::Ymd,
            pad: true,
            clock: Clock::H24,
        }
    }
}

impl DateStyle {
    /// A compact form for `localStorage` and the server setting.
    ///
    /// Three fields joined rather than JSON: it has to be typed into a config
    /// file or an environment variable by hand.
    pub fn id(self) -> String {
        format!(
            "{}-{}-{}",
            self.order.id(),
            if self.pad { "pad" } else { "nopad" },
            self.clock.id()
        )
    }

    /// Parse the compact form, filling in the default for anything missing or
    /// unrecognised.
    ///
    /// Lenient on purpose: a hand-edited or stale value should cost the part
    /// it got wrong, not leave every date on the page unrendered.
    pub fn from_id(id: &str) -> Self {
        let mut style = Self::default();
        for part in id.split('-') {
            match part.trim() {
                "ymd" => style.order = DateOrder::Ymd,
                "dmy" => style.order = DateOrder::Dmy,
                "mdy" => style.order = DateOrder::Mdy,
                "pad" => style.pad = true,
                "nopad" => style.pad = false,
                "24" => style.clock = Clock::H24,
                "12" => style.clock = Clock::H12,
                _ => {}
            }
        }
        style
    }
}

/// A civil date-time, already resolved to the zone it will be shown in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DateParts {
    pub year: i32,
    /// 1-12.
    pub month: u32,
    pub day: u32,
    /// 0-23.
    pub hour: u32,
    pub minute: u32,
}

/// Write already-resolved parts in the chosen style.
///
/// Split from [`local_parts`] so the layouts stay testable: resolving the zone
/// is the browser's job, but how a date reads is ours.
pub fn render(parts: DateParts, style: DateStyle) -> String {
    format!(
        "{} {}",
        render_date(parts, style),
        render_time(parts, style)
    )
}

/// Just the date. A list of builds is mostly scanned by day, and the clock
/// time is noise until you want it — so it lives in hover text instead.
pub fn render_date(parts: DateParts, style: DateStyle) -> String {
    let DateParts {
        year, month, day, ..
    } = parts;

    let field = |value: u32| {
        if style.pad {
            format!("{value:02}")
        } else {
            value.to_string()
        }
    };

    // The year is never abbreviated: a two-digit year is exactly the ambiguity
    // this setting exists to remove.
    match style.order {
        DateOrder::Ymd => format!("{year:04}-{}-{}", field(month), field(day)),
        DateOrder::Dmy => format!("{}/{}/{year:04}", field(day), field(month)),
        DateOrder::Mdy => format!("{}/{}/{year:04}", field(month), field(day)),
    }
}

pub fn render_time(parts: DateParts, style: DateStyle) -> String {
    let DateParts { hour, minute, .. } = parts;

    match style.clock {
        // The 24-hour hour is always padded: `4:05` reads as a 12-hour time.
        Clock::H24 => format!("{hour:02}:{minute:02}"),
        Clock::H12 => {
            let suffix = if hour < 12 { "am" } else { "pm" };
            // Hour 0 is 12 am and hour 12 is 12 pm; a bare `% 12` gets both
            // wrong.
            let hour12 = match hour % 12 {
                0 => 12,
                other => other,
            };
            format!("{hour12}:{minute:02} {suffix}")
        }
    }
}

/// The viewer's timezone, as the browser reports it.
///
/// Asked of the browser for the same reason as [`local_parts`]: it already
/// knows, and its answer is the one every date on the page is rendered in.
pub fn viewer_timezone() -> aurcache_client::Timezone {
    let format = js_sys::Intl::DateTimeFormat::new(&js_sys::Array::new(), &js_sys::Object::new());
    let name = js_sys::Reflect::get(&format.resolved_options(), &"timeZone".into())
        .ok()
        .and_then(|zone| zone.as_string());
    // `getTimezoneOffset` is minutes *behind* UTC: -120 for CEST.
    let behind = js_sys::Date::new_0().get_timezone_offset();
    aurcache_client::Timezone {
        name,
        utc_offset: -(behind as i32) * 60,
    }
}

/// The browser's view of an instant, in the viewer's own timezone.
///
/// Deliberately not computed here. Turning a Unix timestamp into a civil date
/// is fifteen lines; turning it into the *viewer's local* civil date needs a
/// timezone database, which is a large thing to ship into a wasm bundle —
/// measured at +91 KB gzipped for `jiff`, or +34 KB with its tzdb dropped in
/// favour of the browser's offset, against a 374 KB bundle. The browser
/// already has one, and a date crate would ask it the same question anyway.
fn local_parts(ts: i64) -> DateParts {
    let millis = (ts as f64) * 1000.0;
    let date = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(millis));
    DateParts {
        year: date.get_full_year() as i32,
        // `getMonth` is 0-based; nothing else is.
        month: date.get_month() + 1,
        day: date.get_date(),
        hour: date.get_hours(),
        minute: date.get_minutes(),
    }
}

/// An absolute timestamp, in the viewer's chosen style.
pub fn absolute(ts: Option<i64>, style: DateStyle) -> String {
    ts.map_or_else(|| "—".to_string(), |ts| render(local_parts(ts), style))
}

/// How long ago, with the absolute date available on hover.
///
/// Relative reads better for "is this stale", but it hides *when* — so the
/// exact date is one hover away rather than a preference change away.
#[component]
pub fn RelativeDate(ts: Option<i64>, now: i64) -> Element {
    let style = use_date_style();
    let exact = absolute(ts, style());

    rsx! {
        span { title: "{exact}", {crate::format::format_age(ts, now)} }
    }
}

/// An absolute date and time.
#[component]
pub fn AbsoluteDate(ts: Option<i64>) -> Element {
    let style = use_date_style();

    rsx! {
        span { {absolute(ts, style())} }
    }
}

/// Just the date, with the exact time on hover.
///
/// A build list is scanned by day; the minute matters only once something
/// looks wrong, and then it is a hover away.
#[component]
pub fn DateOnly(ts: Option<i64>) -> Element {
    let style = use_date_style();
    // Converted once: `absolute` would run the same `local_parts` conversion
    // a second time for the hover text.
    let parts = ts.map(local_parts);
    let full = parts.map_or_else(|| "—".to_string(), |parts| render(parts, style()));
    let short = parts.map_or_else(|| "—".to_string(), |parts| render_date(parts, style()));

    rsx! {
        span { title: "{full}", "{short}" }
    }
}

fn stored_override() -> Option<DateStyle> {
    web_sys::window()
        .and_then(|w| w.local_storage().ok().flatten())
        .and_then(|s| s.get_item(STORAGE_KEY).ok().flatten())
        .filter(|value| !value.trim().is_empty())
        .map(|value| DateStyle::from_id(&value))
}

fn store_override(style: DateStyle) {
    if let Some(storage) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
        let _ = storage.set_item(STORAGE_KEY, &style.id());
    }
}

/// The style in force, shared by every screen that shows a date.
///
/// A browser that has chosen a style keeps it; one that has not follows the
/// server's `date_format` setting, which is the point of that setting existing.
/// The server value arrives a request later than the first render, so the
/// built-in default fills the gap — and is overwritten only while no local
/// choice exists, or a preference would be undone by a page load.
pub fn use_date_style_provider() -> Signal<DateStyle> {
    let mut style = use_context_provider(|| Signal::new(stored_override().unwrap_or_default()));

    use_future(move || async move {
        if stored_override().is_some() {
            return;
        }
        let Ok(client) = crate::api::client() else {
            return;
        };
        // A server that cannot be reached is not worth reporting here: every
        // screen shows dates, and none of them is about this setting.
        if let Ok(settings) = client.settings(None).await {
            style.set(DateStyle::from_id(&settings.date_format.value));
        }
    });

    style
}

pub fn use_date_style() -> Signal<DateStyle> {
    use_context()
}

/// The browser's own date preference, stored locally.
#[component]
pub fn DateStylePicker() -> Element {
    let mut style = use_date_style();
    let current = style();

    rsx! {
        DateStyleControls {
            value: current,
            onchange: move |next| {
                store_override(next);
                style.set(next);
            },
        }
    }
}

/// The three controls that make up a date style, without deciding where the
/// choice is kept.
///
/// The same widget edits two different things: this browser's local override,
/// and the server-wide default on the settings page. Only the destination
/// differs, so only the destination is the caller's business.
#[component]
pub fn DateStyleControls(value: DateStyle, onchange: EventHandler<DateStyle>) -> Element {
    // Shown against a fixed instant so the options read as examples rather
    // than as abstract patterns.
    let sample = DateParts {
        year: 2026,
        month: 8,
        day: 6,
        hour: 14,
        minute: 5,
    };

    rsx! {
        div { class: "flex flex-col gap-2 py-2",
            label { class: "flex flex-col gap-1",
                span { class: "text-xs opacity-60", "Date order" }
                select {
                    class: "select select-bordered select-sm w-full",
                    onchange: move |e: FormEvent| {
                        let order = DateOrder::ALL
                            .into_iter()
                            .find(|o| o.id() == e.value())
                            .unwrap_or(DateOrder::Ymd);
                        onchange.call(DateStyle { order, ..value });
                    },
                    for order in DateOrder::ALL {
                        option {
                            key: "{order.id()}",
                            value: order.id(),
                            selected: value.order == order,
                            "{order.label()}"
                        }
                    }
                }
            }

            label { class: "flex flex-col gap-1",
                span { class: "text-xs opacity-60", "Time" }
                select {
                    class: "select select-bordered select-sm w-full",
                    onchange: move |e: FormEvent| {
                        let clock = Clock::ALL
                            .into_iter()
                            .find(|c| c.id() == e.value())
                            .unwrap_or(Clock::H24);
                        onchange.call(DateStyle { clock, ..value });
                    },
                    for clock in Clock::ALL {
                        option {
                            key: "{clock.id()}",
                            value: clock.id(),
                            selected: value.clock == clock,
                            "{clock.label()}"
                        }
                    }
                }
            }

            label { class: "flex items-center gap-2 cursor-pointer",
                input {
                    r#type: "checkbox",
                    class: "checkbox checkbox-sm",
                    checked: value.pad,
                    onchange: move |e: FormEvent| {
                        onchange.call(DateStyle { pad: e.checked(), ..value });
                    },
                }
                span { class: "text-sm", "Pad day and month with zeros" }
            }

            span { class: "text-xs opacity-50 font-mono", {render(sample, value)} }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Clock, DateOrder, DateParts, DateStyle, render};

    /// 2026-08-06 14:05 — a single-digit day, so padding is visible.
    const PARTS: DateParts = DateParts {
        year: 2026,
        month: 8,
        day: 6,
        hour: 14,
        minute: 5,
    };

    fn style(order: DateOrder, pad: bool, clock: Clock) -> DateStyle {
        DateStyle { order, pad, clock }
    }

    #[test]
    fn each_order_puts_the_fields_where_its_label_says() {
        assert_eq!(
            render(PARTS, style(DateOrder::Ymd, true, Clock::H24)),
            "2026-08-06 14:05"
        );
        assert_eq!(
            render(PARTS, style(DateOrder::Dmy, true, Clock::H24)),
            "06/08/2026 14:05"
        );
        assert_eq!(
            render(PARTS, style(DateOrder::Mdy, true, Clock::H24)),
            "08/06/2026 14:05"
        );
    }

    #[test]
    fn padding_applies_to_day_and_month_but_never_the_year() {
        assert_eq!(
            render(PARTS, style(DateOrder::Mdy, false, Clock::H24)),
            "8/6/2026 14:05"
        );
        assert_eq!(
            render(PARTS, style(DateOrder::Ymd, false, Clock::H24)),
            "2026-8-6 14:05"
        );
    }

    /// The clock is independent of the date order: any combination works.
    #[test]
    fn the_clock_is_independent_of_the_date_order() {
        for order in DateOrder::ALL {
            let rendered = render(PARTS, style(order, true, Clock::H12));
            assert!(rendered.ends_with("2:05 pm"), "{rendered}");
        }
    }

    /// Midnight and noon are where a 12-hour clock goes wrong: hour 0 is
    /// 12 am, not 0 am, and hour 12 is 12 pm, not 0 pm.
    #[test]
    fn the_twelve_hour_clock_handles_midnight_and_noon() {
        let midnight = DateParts {
            hour: 0,
            minute: 0,
            ..PARTS
        };
        let noon = DateParts {
            hour: 12,
            minute: 0,
            ..PARTS
        };
        assert!(
            render(midnight, style(DateOrder::Ymd, true, Clock::H12)).ends_with("12:00 am"),
            "midnight"
        );
        assert!(
            render(noon, style(DateOrder::Ymd, true, Clock::H12)).ends_with("12:00 pm"),
            "noon"
        );
    }

    /// The 24-hour clock always pads the hour, whatever the date padding is:
    /// `4:05` would be ambiguous against a 12-hour reading.
    #[test]
    fn the_24_hour_clock_always_pads_the_hour() {
        let early = DateParts { hour: 4, ..PARTS };
        let rendered = render(early, style(DateOrder::Ymd, false, Clock::H24));
        assert!(rendered.ends_with("04:05"), "{rendered}");
    }

    #[test]
    fn a_style_round_trips_through_its_stored_form() {
        for order in DateOrder::ALL {
            for pad in [true, false] {
                for clock in Clock::ALL {
                    let original = style(order, pad, clock);
                    assert_eq!(DateStyle::from_id(&original.id()), original);
                }
            }
        }
    }

    /// A stale or hand-edited value costs the part it got wrong, not the
    /// whole setting.
    #[test]
    fn an_unrecognised_stored_value_falls_back_field_by_field() {
        assert_eq!(DateStyle::from_id(""), DateStyle::default());
        assert_eq!(DateStyle::from_id("nonsense"), DateStyle::default());
        // Only the clock is recognisable here; the rest defaults.
        let partial = DateStyle::from_id("bogus-12");
        assert_eq!(partial.clock, Clock::H12);
        assert_eq!(partial.order, DateStyle::default().order);
    }
}
