//! Sorting and filtering for the package and build lists.
//!
//! Done in the browser over the whole fetched list rather than as query
//! parameters. Both lists are fetched entire, so filtering, sorting and paging
//! stay instant, and each of the three sees the results of the other two —
//! server-side paging would mean a filter that only searched the page you were
//! already on. If the fetch itself ever becomes the problem, the pure functions
//! here are what would move.

use aurcache_client::{Build, SimplePackage};
use aurcache_common::build_state::BuildState;
use std::cmp::Reverse;

/// Which column a list is ordered by.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SortKey {
    Name,
    Status,
    Time,
    Size,
    /// The worker that ran a build. Builds only; packages have no worker.
    Worker,
    /// How long a build ran, from the timestamps it records.
    Duration,
    /// A build's peak memory, from its cgroup. Builds only.
    Memory,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SortDir {
    Asc,
    Desc,
}

impl SortDir {
    #[must_use]
    pub fn flipped(self) -> Self {
        match self {
            Self::Asc => Self::Desc,
            Self::Desc => Self::Asc,
        }
    }

    /// The arrow shown on the active column.
    pub fn arrow(self) -> &'static str {
        match self {
            Self::Asc => "▲",
            Self::Desc => "▼",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Sort {
    pub key: SortKey,
    pub dir: SortDir,
}

impl Sort {
    /// Clicking a column sorts by it; clicking the active one reverses.
    ///
    /// A new column starts descending for time — the newest build is what you
    /// want first — and ascending for everything else, where A-Z is the
    /// natural reading.
    #[must_use]
    pub fn toggled(self, key: SortKey) -> Self {
        if self.key == key {
            Self {
                key,
                dir: self.dir.flipped(),
            }
        } else {
            Self {
                key,
                dir: match key {
                    // Newest build, largest package, longest build, highest
                    // peak first: for these the interesting end is the top,
                    // where A-Z is the natural reading for everything else.
                    SortKey::Time | SortKey::Size | SortKey::Duration | SortKey::Memory => {
                        SortDir::Desc
                    }
                    _ => SortDir::Asc,
                },
            }
        }
    }
}

/// Which build states a status filter holds, as a bitset over the six
/// `BuildState`s. Empty means "any state" rather than "no state", so the
/// default filter stays the zero value. Stays `Copy` so list controls can
/// hold it in a signal like the single-valued filter did.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct StateSet(u8);

impl StateSet {
    const fn bit(state: BuildState) -> u8 {
        1 << (state.as_i32() as u8)
    }

    #[must_use]
    pub fn empty() -> Self {
        Self(0)
    }

    #[must_use]
    pub fn from_slice(states: &[BuildState]) -> Self {
        let mut set = Self::empty();
        for &state in states {
            set.insert(state);
        }
        set
    }

    pub fn insert(&mut self, state: BuildState) {
        self.0 |= Self::bit(state);
    }

    pub fn remove(&mut self, state: BuildState) {
        self.0 &= !Self::bit(state);
    }

    #[must_use]
    pub fn contains(self, state: BuildState) -> bool {
        self.0 & Self::bit(state) != 0
    }

    #[must_use]
    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    #[must_use]
    pub fn len(self) -> usize {
        self.0.count_ones() as usize
    }

    /// The held states in discriminant order, so a URL encoding is stable.
    pub fn iter(self) -> impl Iterator<Item = BuildState> {
        BuildState::ALL
            .into_iter()
            .filter(move |state| self.contains(*state))
    }
}

/// How a status filter is expressed: a set of build states, plus the packages
/// with something newer available upstream. The last is not a build state --
/// a package's last build is still `Successful` when it is out of date -- so
/// it is a flag of its own, backed by `SimplePackage` `out_of_date` rather than
/// by the `status` column. The builds list has no such flag, so the dropdown
/// only offers it where it means something.
///
/// Empty (no states, no flag) means everything. A row matches when it meets
/// any selected condition, so the Packages page gets "failed or out of date".
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct StatusFilter {
    pub states: StateSet,
    pub out_of_date: bool,
}

impl StatusFilter {
    /// Everything: no states, no flag.
    pub const ANY: Self = Self {
        states: StateSet(0),
        out_of_date: false,
    };

    /// Only the out-of-date pseudo-status.
    pub const OUTDATED: Self = Self {
        states: StateSet(0),
        out_of_date: true,
    };

    /// Exactly one build state.
    #[must_use]
    pub fn with_state(state: BuildState) -> Self {
        Self {
            states: StateSet::from_slice(&[state]),
            out_of_date: false,
        }
    }

    /// Exactly these build states.
    #[must_use]
    pub fn with_states(states: &[BuildState]) -> Self {
        Self {
            states: StateSet::from_slice(states),
            out_of_date: false,
        }
    }

    /// Whether this filter selects everything.
    #[must_use]
    pub fn is_any(self) -> bool {
        self.states.is_empty() && !self.out_of_date
    }

    /// How many conditions are selected, for the dropdown button summary.
    #[must_use]
    pub fn selected_count(self) -> usize {
        self.states.len() + usize::from(self.out_of_date)
    }

    /// The filter with one state's membership flipped.
    #[must_use]
    pub fn toggled_state(self, state: BuildState) -> Self {
        let mut states = self.states;
        if states.contains(state) {
            states.remove(state);
        } else {
            states.insert(state);
        }
        Self {
            states,
            out_of_date: self.out_of_date,
        }
    }

    /// The filter with the out-of-date flag flipped.
    #[must_use]
    pub fn toggled_outdated(self) -> Self {
        Self {
            states: self.states,
            out_of_date: !self.out_of_date,
        }
    }

    /// How this filter is spelled in a URL: the selected states' keys,
    /// comma-joined, as the server's `?status=` takes them.
    ///
    /// A URL is read and shared: `?s=failed` and `?s=enqueued,waiting-for-deps`
    /// say what `?s=2` does not.
    #[must_use]
    pub fn slug(self) -> String {
        let mut parts: Vec<&'static str> = self.states.iter().map(BuildState::key).collect();
        if self.out_of_date {
            parts.push(OUTDATED_SLUG);
        }
        parts.join(",")
    }

    /// Parse [`Self::slug`]. An unknown slug is dropped rather than turning the
    /// whole filter to any, so a hand-edited URL still shows a list rather than
    /// an error page; one with nothing recognisable is everything, which is
    /// what an empty filter already means.
    #[must_use]
    pub fn from_slug(value: &str) -> Self {
        value
            .split(',')
            .map(str::trim)
            .fold(Self::ANY, |filter, part| match part {
                OUTDATED_SLUG => Self {
                    out_of_date: true,
                    ..filter
                },
                _ => match BuildState::from_key(part) {
                    Some(state) => {
                        let mut states = filter.states;
                        states.insert(state);
                        Self { states, ..filter }
                    }
                    None => filter,
                },
            })
    }

    /// Whether the underlying state and out-of-date flag answer to this filter.
    fn matches(self, status: BuildState, out_of_date: bool) -> bool {
        if self.is_any() {
            return true;
        }
        // The same condition the badge calls "out of date": a successful
        // build that newer sources are ahead of. The flag is only ever
        // meaningful for such a package in practice, but matching the badge
        // keeps a filter result and a row's label from disagreeing.
        if self.out_of_date && status == BuildState::Successful && out_of_date {
            return true;
        }
        self.states.contains(status)
    }
}

/// The out-of-date pseudo-status in a URL, beside the build states' keys.
const OUTDATED_SLUG: &str = "outdated";

/// The dropdown button text for a status filter: what is picked, or how many.
#[must_use]
pub fn filter_summary(status: StatusFilter) -> String {
    match status.selected_count() {
        0 => "Any status".to_string(),
        // Name the one thing picked.
        1 => status
            .states
            .iter()
            .next()
            .map_or_else(|| "Out of date".to_string(), state_label),
        n => format!("{n} statuses"),
    }
}

/// A state as a filter option names it: its label, capitalised.
#[must_use]
pub fn state_label(state: BuildState) -> String {
    let label = state.label();
    let mut chars = label.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(chars).collect()
    })
}

/// Case-insensitive substring match.
///
/// Lowered once per call, not once per row: the filters below run this over
/// every row on every keystroke.
///
/// Substring rather than prefix because package names are compound —
/// searching `gtk` should find `lib32-gtk3`, which a prefix match would miss.
fn lowered_query(query: &str) -> String {
    query.trim().to_lowercase()
}

/// Order two statuses so the ones needing attention come first.
///
/// Not the numeric order of the enum, which is an implementation detail:
/// sorting by status is asking "what needs me", so failures lead and
/// up-to-date packages trail.
fn status_rank(status: BuildState) -> u8 {
    match status {
        BuildState::Failed => 0,
        BuildState::Active | BuildState::Publishing => 1,
        BuildState::WaitingForDeps => 2,
        BuildState::Enqueued => 3,
        BuildState::Successful => 4,
    }
}

/// Every state, in the order a status sort puts them: what the status filter
/// lists, so its options read in the same order as the column they filter.
#[must_use]
pub fn states_by_attention() -> [BuildState; 6] {
    let mut states = BuildState::ALL;
    // Stable, so states of equal rank keep their discriminant order.
    states.sort_by_key(|state| status_rank(*state));
    states
}

/// Sort `items` by `key` in `dir`.
///
/// The key is computed once per row rather than once per comparison, which
/// matters for the lowered names most columns sort by. Both directions keep
/// ties in the order they arrived in: descending reverses the key, not the
/// result.
fn by_dir<T, K: Ord>(items: &mut [T], dir: SortDir, mut key: impl FnMut(&T) -> K) {
    match dir {
        SortDir::Asc => items.sort_by_cached_key(key),
        SortDir::Desc => items.sort_by_cached_key(|item| Reverse(key(item))),
    }
}

pub fn filter_packages<'a>(
    packages: impl IntoIterator<Item = &'a SimplePackage>,
    query: &str,
    status: StatusFilter,
) -> Vec<SimplePackage> {
    let query = lowered_query(query);
    packages
        .into_iter()
        .filter(|pkg| {
            (query.is_empty() || pkg.name.to_lowercase().contains(&query))
                && status.matches(pkg.status, pkg.out_of_date)
        })
        .cloned()
        .collect()
}

pub fn sort_packages(packages: &mut [SimplePackage], sort: Sort) {
    match sort.key {
        // An out-of-date package is a package needing attention, so it ranks
        // with the unhealthy ones rather than with the successes.
        SortKey::Status => by_dir(packages, sort.dir, |p| {
            (status_rank(p.status), !p.out_of_date)
        }),
        // `Option`'s own ordering is what this wants: `None` sorts below every
        // `Some`, so unrecorded sizes group at one end rather than among the
        // small ones, and land last under the descending order a Size column
        // opens in.
        SortKey::Size => by_dir(packages, sort.dir, |p| p.total_size),
        // A package row carries no timestamp, worker, duration or peak memory
        // of its own, so those headers fall back to the name rather than
        // presenting an ordering that has nothing to do with the column.
        SortKey::Name | SortKey::Time | SortKey::Worker | SortKey::Duration | SortKey::Memory => {
            by_dir(packages, sort.dir, |p| p.name.to_lowercase());
        }
    }
}

pub fn filter_builds(builds: &[Build], query: &str, status: StatusFilter) -> Vec<Build> {
    let query = lowered_query(query);
    builds
        .iter()
        .filter(|build| {
            // The identity string is only built when something is typed: it
            // allocates per row, and an empty query matches everything anyway.
            (query.is_empty() || build_id(build).to_lowercase().contains(&query))
                && status.matches(build.status, false)
        })
        .cloned()
        .collect()
}

/// A build's identity, as the list shows it and as people refer to it.
///
/// Filtering matches this rather than the package name alone, so both halves of
/// what is on screen are searchable: `hello` finds every build of it, because
/// the match is a substring one, and `hello/3` finds the one. Matching only the
/// package meant typing what the row said found nothing.
fn build_id(build: &Build) -> String {
    match build.worker_name.as_deref() {
        // The worker is part of what the row shows, so it is part of what the
        // filter matches -- the same rule that put the build number in here.
        // It is also what the Workers page links to, which is how "show me
        // this machine's builds" is spelled.
        Some(worker) => format!("{}/{} {worker}", build.pkg_name, build.number),
        None => format!("{}/{}", build.pkg_name, build.number),
    }
}

pub fn sort_builds(builds: &mut [Build], sort: Sort) {
    match sort.key {
        // A package's own builds are then newest-first, so the groups read as
        // histories rather than as an arbitrary jumble.
        SortKey::Name => by_dir(builds, sort.dir, |b| {
            (b.pkg_name.to_lowercase(), Reverse(b.number))
        }),
        // Unclaimed builds have no worker. `None` sorts before `Some`, so
        // ascending puts the queue first and descending puts it last -- either
        // way they group together rather than scattering. The whole key is
        // reversed, not each part: reversing only the name inside the `Option`
        // would keep the queue first. Within one worker, newest first.
        SortKey::Worker => by_dir(builds, sort.dir, |b| {
            (
                b.worker_name.as_deref().map(str::to_lowercase),
                Reverse(b.number),
            )
        }),
        SortKey::Status => by_dir(builds, sort.dir, |b| status_rank(b.status)),
        SortKey::Time => by_dir(builds, sort.dir, |b| b.start_time),
        SortKey::Size => by_dir(builds, sort.dir, |b| b.size),
        // A build still running has no end yet, so no duration. `None` sorts
        // before `Some`, which under the descending order a Duration column
        // opens in puts the unfinished builds last, after the longest -- the
        // interesting end first. Ties are newest first for a stable readout.
        SortKey::Duration => by_dir(builds, sort.dir, |b| (b.elapsed(), Reverse(b.number))),
        // As with size, unrecorded peaks group at one end: `None` under every
        // `Some`, and last under the descending order a Memory column opens in.
        SortKey::Memory => by_dir(builds, sort.dir, |b| (b.peak_memory, Reverse(b.number))),
    }
}

/// How many rows a page holds.
pub const PAGE_SIZE: usize = 100;

/// One page of `items`, with the page number that was actually used.
///
/// The page is clamped rather than trusted. Narrowing a filter while on a later
/// page leaves the requested page past the end of a now-shorter list, and an
/// unclamped slice would render an empty table with no indication that the rows
/// are simply somewhere else.
#[must_use]
pub fn paginate<T: Clone>(items: &[T], page: usize) -> Page<T> {
    let info = PageInfo::at(page, items.len());
    Page {
        items: items[info.first..info.first + info.count].to_vec(),
        info,
    }
}

/// One page of a list, and where it sits in the whole.
///
/// Comparable so memoized pipelines can hold it: [`use_memo`] only keeps a
/// value it can tell apart from the last one.
///
/// [`use_memo`]: dioxus::prelude::use_memo
#[derive(PartialEq)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub info: PageInfo,
}

/// Where a page sits in a list: everything the pager shows, without the rows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PageInfo {
    /// Zero-based, and clamped to the list that actually exists.
    pub index: usize,
    /// At least one, so "page 1 of 1" is what an empty list reads as.
    pub pages: usize,
    /// Index of the first row on this page, for the human-readable range.
    pub first: usize,
    /// How many rows this page holds.
    pub count: usize,
    pub total: usize,
}

impl PageInfo {
    /// Page `page` of a list of `total` rows, clamped to the last page there
    /// is. Every field follows from that one clamped index, so the range and
    /// the page number cannot disagree.
    #[must_use]
    pub fn at(page: usize, total: usize) -> Self {
        let pages = total.div_ceil(PAGE_SIZE).max(1);
        let index = page.min(pages - 1);
        let first = index * PAGE_SIZE;
        Self {
            index,
            pages,
            first,
            count: (total - first).min(PAGE_SIZE),
            total,
        }
    }
}

impl SortKey {
    /// The spelling used in a URL.
    #[must_use]
    pub fn slug(self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Status => "status",
            Self::Time => "time",
            Self::Size => "size",
            Self::Worker => "worker",
            Self::Duration => "duration",
            Self::Memory => "memory",
        }
    }

    fn from_slug(slug: &str) -> Option<Self> {
        match slug {
            "name" => Some(Self::Name),
            "status" => Some(Self::Status),
            "time" => Some(Self::Time),
            "size" => Some(Self::Size),
            "worker" => Some(Self::Worker),
            "duration" => Some(Self::Duration),
            "memory" => Some(Self::Memory),
            _ => None,
        }
    }
}

impl SortDir {
    #[must_use]
    pub fn slug(self) -> &'static str {
        match self {
            Self::Asc => "asc",
            Self::Desc => "desc",
        }
    }

    fn from_slug(slug: &str) -> Option<Self> {
        match slug {
            "asc" => Some(Self::Asc),
            "desc" => Some(Self::Desc),
            _ => None,
        }
    }
}

/// The filter and sort of a list, carried in the URL's query string.
///
/// A spread query segment (`?:..view`), so the *whole* query is this type and
/// its encoding is ours. That is what makes it usable at all: dioxus writes a
/// named parameter as `name=` whether or not it has a value, so `?:status`
/// alone put a dangling `?status=` on every unfiltered URL. Owning the string
/// means a default view writes nothing.
///
/// One `?` survives, because the generated `Display` emits it before this type
/// is consulted (`router-macro/src/query.rs`, `write!(f, "?{}", ...)`), so an
/// unfiltered list is `/packages?`. That is DioxusLabs/dioxus#5792, and #5793
/// is the one-line fix; when it lands the trailing `?` disappears with no
/// change here. `an_empty_search_leaves_no_trace_in_the_url` allows it and
/// nothing else.
///
/// Values are comma-joined slugs, since a URL is read and shared:
/// `?s=failed&o=name-desc` and `?s=enqueued,waiting-for-deps` say what `?s=2`
/// does not.
/// Neither `&` nor `=` is in dioxus's `QUERY_ASCII_SET`, so they survive
/// unescaped and the query stays legible.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct ViewParams {
    pub status: Option<StatusFilter>,
    /// `None` means "the list's own default order", so the common view carries
    /// no `o=` at all rather than spelling out what it would have done anyway.
    pub sort: Option<Sort>,
    pub dependencies: bool,
}

impl ViewParams {
    /// Build from live control state, dropping anything that matches the
    /// list's default so an untouched list has a clean URL.
    #[must_use]
    pub fn from_state(
        status: StatusFilter,
        sort: Sort,
        default_sort: Sort,
        dependencies: bool,
    ) -> Self {
        Self {
            status: (!status.is_any()).then_some(status),
            sort: (sort != default_sort).then_some(sort),
            dependencies,
        }
    }

    /// The sort to apply, given what this list would use by default.
    #[must_use]
    pub fn sort_or(&self, default_sort: Sort) -> Sort {
        self.sort.unwrap_or(default_sort)
    }

    #[must_use]
    pub fn status_filter(&self) -> StatusFilter {
        self.status.unwrap_or(StatusFilter::ANY)
    }

    /// A view filtered to one status and nothing else, for links into a list.
    ///
    /// `dependencies: true`: ignored by the Builds list, which has no such
    /// toggle, but load-bearing for the Packages list -- a dashboard card can
    /// include a dependency package (a failed build blocks its parent
    /// regardless of who asked for it), and the link this builds has to show
    /// the same rows the card counted, or "View all" lands on a list missing
    /// the very package that sent the reader there.
    #[must_use]
    pub fn with_status(status: BuildState) -> Self {
        Self {
            status: Some(StatusFilter::with_state(status)),
            dependencies: true,
            ..Self::default()
        }
    }

    /// A view filtered to these statuses and nothing else, for links into a
    /// list — the stuck queue lands on Builds with both queued states ticked.
    #[must_use]
    pub fn with_states(statuses: &[BuildState]) -> Self {
        Self {
            status: Some(StatusFilter::with_states(statuses)),
            ..Self::default()
        }
    }

    /// A view filtered to out-of-date packages and nothing else.
    ///
    /// `dependencies: true`, for the same reason as [`Self::with_status`]: a
    /// dependency package can be out of date too.
    #[must_use]
    pub fn with_out_of_date() -> Self {
        Self {
            status: Some(StatusFilter::OUTDATED),
            dependencies: true,
            ..Self::default()
        }
    }

    /// A view sorted by this column and nothing else, for links into a list.
    #[must_use]
    pub fn with_sort(key: SortKey, dir: SortDir) -> Self {
        Self {
            sort: Some(Sort { key, dir }),
            ..Self::default()
        }
    }
}

impl std::fmt::Display for ViewParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut sep = "";
        if let Some(status) = self.status
            && !status.is_any()
        {
            write!(f, "s={}", status.slug())?;
            sep = "&";
        }
        if let Some(sort) = self.sort {
            write!(f, "{sep}o={}-{}", sort.key.slug(), sort.dir.slug())?;
            sep = "&";
        }
        if self.dependencies {
            write!(f, "{sep}d=1")?;
        }
        Ok(())
    }
}

impl From<&str> for ViewParams {
    /// Parse what [`Display`] writes. Unrecognised keys and values are ignored
    /// rather than rejected: a hand-edited or outdated URL should still show a
    /// list, and `FromQuery` has nowhere to report an error to anyway.
    fn from(query: &str) -> Self {
        let mut view = Self::default();
        for (key, value) in query.split('&').filter_map(|p| p.split_once('=')) {
            match key {
                "s" => {
                    let filter = StatusFilter::from_slug(value);
                    view.status = (!filter.is_any()).then_some(filter);
                }
                "o" => {
                    if let Some((key, dir)) = value.split_once('-')
                        && let (Some(key), Some(dir)) =
                            (SortKey::from_slug(key), SortDir::from_slug(dir))
                    {
                        view.sort = Some(Sort { key, dir });
                    }
                }
                "d" => view.dependencies = value == "1",
                _ => {}
            }
        }
        view
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A default view writes nothing, which is what keeps `/packages?` from
    /// becoming `/packages?s=any&o=name-asc&d=0`.
    #[test]
    fn a_default_view_encodes_to_nothing() {
        assert_eq!(ViewParams::default().to_string(), "");
    }

    /// Only what differs from the list's own default is written, so an
    /// untouched list has a clean URL and a changed one says what changed.
    #[test]
    fn only_non_default_state_reaches_the_url() {
        let default_sort = Sort {
            key: SortKey::Name,
            dir: SortDir::Asc,
        };
        let untouched =
            ViewParams::from_state(StatusFilter::ANY, default_sort, default_sort, false);
        assert_eq!(untouched.to_string(), "");

        let changed = ViewParams::from_state(
            StatusFilter::with_state(BuildState::Failed),
            Sort {
                key: SortKey::Worker,
                dir: SortDir::Desc,
            },
            default_sort,
            true,
        );
        assert_eq!(changed.to_string(), "s=failed&o=worker-desc&d=1");
    }

    /// Whatever `Display` writes must parse back to the same view: the URL is
    /// the only place this state lives across a reload.
    #[test]
    fn a_view_round_trips_through_its_query() {
        for view in [
            ViewParams::default(),
            ViewParams::with_status(BuildState::Active),
            ViewParams {
                status: Some(StatusFilter::OUTDATED),
                sort: None,
                dependencies: false,
            },
            ViewParams::with_states(&[BuildState::Enqueued, BuildState::WaitingForDeps]),
            ViewParams {
                status: Some(StatusFilter {
                    states: StateSet::from_slice(&[BuildState::Failed]),
                    out_of_date: true,
                }),
                sort: None,
                dependencies: false,
            },
            ViewParams {
                status: Some(StatusFilter::with_state(BuildState::Failed)),
                sort: Some(Sort {
                    key: SortKey::Size,
                    dir: SortDir::Desc,
                }),
                dependencies: true,
            },
        ] {
            let encoded = view.to_string();
            assert_eq!(ViewParams::from(encoded.as_str()), view, "{encoded:?}");
        }
    }

    /// A hand-edited or outdated URL shows a list rather than an error: unknown
    /// keys and values fall back to the default instead of being rejected.
    #[test]
    fn an_unparsable_query_falls_back_to_the_default_view() {
        assert_eq!(ViewParams::from("s=nonsense"), ViewParams::default());
        assert_eq!(ViewParams::from("o=bogus-sideways"), ViewParams::default());
        assert_eq!(ViewParams::from("whatever"), ViewParams::default());
        // A key we do not know is ignored, and the rest still applies.
        assert_eq!(
            ViewParams::from("zz=1&s=failed").status,
            Some(StatusFilter::with_state(BuildState::Failed))
        );
        // An unknown slug among known ones is dropped; a value of only
        // unknown slugs is everything.
        assert_eq!(
            ViewParams::from("s=failed,bogus").status,
            Some(StatusFilter::with_state(BuildState::Failed))
        );
        assert_eq!(ViewParams::from("s=bogus"), ViewParams::default());
        // A set round-trips as comma-joined slugs.
        assert_eq!(
            ViewParams::from("s=enqueued,waiting-for-deps").status,
            Some(StatusFilter::with_states(&[
                BuildState::Enqueued,
                BuildState::WaitingForDeps
            ]))
        );
    }

    fn package(name: &str, status: BuildState, out_of_date: bool) -> SimplePackage {
        SimplePackage {
            name: name.to_string(),
            // These tests are about the search and sort helpers, which do not
            // look at it; the packages screen owns the dependency filter.
            directly_requested: true,
            status,
            out_of_date,
            built_version: None,
            upstream_version: None,
            total_size: None,
        }
    }

    fn sized(name: &str, total_size: Option<i64>) -> SimplePackage {
        SimplePackage {
            total_size,
            ..package(name, BuildState::Successful, false)
        }
    }

    /// Descending is what a Size column opens in, and it puts the biggest first
    /// with the unrecorded ones trailing -- below even a zero-byte package,
    /// since `None` sorts below every `Some`.
    #[test]
    fn sorting_by_size_puts_the_largest_first_and_the_unknown_last() {
        let mut packages = vec![
            sized("small", Some(10)),
            sized("unknown", None),
            sized("large", Some(9000)),
            sized("empty", Some(0)),
        ];
        sort_packages(
            &mut packages,
            Sort {
                key: SortKey::Size,
                dir: SortDir::Desc,
            },
        );
        let order: Vec<&str> = packages.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(order, ["large", "small", "empty", "unknown"]);
    }

    /// Reversing moves the unknowns to the other end as a group; they are never
    /// interleaved with real sizes in either direction.
    #[test]
    fn reversing_the_size_sort_keeps_the_unknown_together() {
        let mut packages = vec![
            sized("small", Some(10)),
            sized("unknown", None),
            sized("large", Some(9000)),
        ];
        sort_packages(
            &mut packages,
            Sort {
                key: SortKey::Size,
                dir: SortDir::Asc,
            },
        );
        let order: Vec<&str> = packages.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(order, ["unknown", "small", "large"]);
    }

    /// A Size column opens descending: the big ones are the reason to sort by
    /// size at all, so they should be on screen without a second click.
    #[test]
    fn size_opens_descending() {
        let sort = Sort {
            key: SortKey::Name,
            dir: SortDir::Asc,
        };
        assert_eq!(sort.toggled(SortKey::Size).dir, SortDir::Desc);
    }

    /// A page is a window on the list, not a prefix of it.
    #[test]
    fn a_page_is_the_slice_it_says_it_is() {
        let items: Vec<usize> = (0..250).collect();

        let first = paginate(&items, 0);
        assert_eq!(first.items.len(), PAGE_SIZE);
        assert_eq!(first.items[0], 0);
        assert_eq!(first.info.index, 0);
        assert_eq!(first.info.pages, 3);
        assert_eq!(first.info.first, 0);
        assert_eq!(first.info.total, 250);

        let second = paginate(&items, 1);
        assert_eq!(second.items[0], PAGE_SIZE);
        assert_eq!(second.info.first, PAGE_SIZE);

        // The last page is the remainder, not a short read of a full one.
        let last = paginate(&items, 2);
        assert_eq!(last.items.len(), 50);
        assert_eq!(last.items[0], 200);
    }

    /// Narrowing a filter while on a later page leaves the requested page past
    /// the end. Unclamped that renders an empty table, which reads as "nothing
    /// matches" when the rows are merely somewhere else.
    #[test]
    fn a_page_past_the_end_falls_back_to_the_last_one() {
        let items: Vec<usize> = (0..5).collect();
        let page = paginate(&items, 7);

        assert_eq!(page.info.index, 0);
        assert_eq!(page.info.pages, 1);
        assert_eq!(page.items.len(), 5);
    }

    /// A page past the end is described from the clamped page throughout: the
    /// range and the page number come from one index, so they cannot disagree.
    #[test]
    fn a_page_past_the_end_is_described_as_the_last_one() {
        let info = PageInfo::at(9, 250);
        assert_eq!(info.index, 2);
        assert_eq!(info.first, 200);
        assert_eq!(info.count, 50);
        assert_eq!(info.pages, 3);
    }

    /// An empty list is one empty page, so the pager reads "1 of 1" rather
    /// than dividing by zero on the way to saying so.
    #[test]
    fn an_empty_list_is_a_single_empty_page() {
        let page = paginate::<usize>(&[], 3);

        assert_eq!(page.info.pages, 1);
        assert_eq!(page.info.index, 0);
        assert!(page.items.is_empty());
        assert_eq!(page.info.total, 0);
    }

    /// The list shows `package/number`, so that is what typing it must match.
    /// Matching the package name alone meant copying what a row said found
    /// nothing at all.
    #[test]
    fn builds_are_filtered_by_the_name_the_list_shows() {
        let builds = vec![
            build(3, "hello", BuildState::Successful, Some(30)),
            build(4, "hello", BuildState::Successful, Some(40)),
            build(3, "neofetch", BuildState::Successful, Some(50)),
        ];

        // The package alone still finds all of its builds.
        let by_package = filter_builds(&builds, "hello", StatusFilter::ANY);
        assert_eq!(by_package.len(), 2);

        // And the identity finds the one.
        let by_id = filter_builds(&builds, "hello/4", StatusFilter::ANY);
        assert_eq!(by_id.len(), 1);
        assert_eq!(by_id[0].number, 4);

        // A build of another package with the same number is not it.
        let other = filter_builds(&builds, "neofetch/3", StatusFilter::ANY);
        assert_eq!(other.len(), 1);
        assert_eq!(other[0].pkg_name, "neofetch");

        assert!(filter_builds(&builds, "hello/9", StatusFilter::ANY).is_empty());
    }

    fn build(number: i32, pkg: &str, status: BuildState, start: Option<i64>) -> Build {
        Build {
            number,
            pkg_name: pkg.to_string(),
            version: "1.0-1".to_string(),
            status,
            start_time: start,
            end_time: None,
            platform: "x86_64".to_string(),
            size: None,
            peak_memory: None,
            disk_usage: None,
            kept: None,
            worker_name: None,
            log_size: None,
            waiting_reason: None,
        }
    }

    /// Searching by a fragment has to find compound names, which is most of
    /// them — `gtk` should find `lib32-gtk3`.
    #[test]
    fn searching_matches_anywhere_in_the_name() {
        let packages = vec![
            package("gtk3", BuildState::Successful, false),
            package("lib32-gtk3", BuildState::Successful, false),
            package("firefox", BuildState::Successful, false),
        ];
        let found = filter_packages(&packages, "gtk", StatusFilter::ANY);
        assert_eq!(found.len(), 2);
        assert!(found.iter().all(|p| p.name.contains("gtk")));
    }

    #[test]
    fn searching_ignores_case_and_surrounding_space() {
        let packages = vec![package("Firefox", BuildState::Successful, false)];
        for query in ["firefox", "FIREFOX", "  fire  "] {
            assert_eq!(
                filter_packages(&packages, query, StatusFilter::ANY).len(),
                1,
                "{query:?}"
            );
        }
    }

    /// An empty query is not a filter — it must not hide everything.
    #[test]
    fn an_empty_query_keeps_everything() {
        let packages = vec![
            package("a", BuildState::Successful, false),
            package("b", BuildState::Failed, false),
        ];
        assert_eq!(filter_packages(&packages, "", StatusFilter::ANY).len(), 2);
        assert_eq!(
            filter_packages(&packages, "   ", StatusFilter::ANY).len(),
            2
        );
    }

    #[test]
    fn the_status_filter_selects_one_state() {
        let packages = vec![
            package("a", BuildState::Successful, false),
            package("b", BuildState::Failed, false),
            package("c", BuildState::Failed, false),
        ];
        let failed = filter_packages(&packages, "", StatusFilter::with_state(BuildState::Failed));
        assert_eq!(failed.len(), 2);
        assert_eq!(filter_packages(&packages, "", StatusFilter::ANY).len(), 3);
    }

    #[test]
    fn the_status_filter_selects_any_of_several_states() {
        let packages = vec![
            package("a", BuildState::Successful, false),
            package("b", BuildState::Failed, false),
            package("c", BuildState::Enqueued, false),
            package("d", BuildState::Active, false),
        ];
        // The stuck queue spans two states; both answer to one filter.
        let queued = filter_packages(
            &packages,
            "",
            StatusFilter::with_states(&[BuildState::Enqueued, BuildState::WaitingForDeps]),
        );
        assert_eq!(
            queued.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
            ["c"]
        );
        let either = filter_packages(
            &packages,
            "",
            StatusFilter::with_states(&[BuildState::Failed, BuildState::Enqueued]),
        );
        assert_eq!(either.len(), 2);
    }

    /// "Out of date" is a pseudo-status of its own, not a build state: only
    /// the packages whose successful build newer sources are ahead of answer
    /// to it.
    #[test]
    fn the_out_of_date_filter_selects_packages_behind_upstream() {
        let packages = vec![
            package("stale", BuildState::Successful, true),
            package("current", BuildState::Successful, false),
            package("broken", BuildState::Failed, true),
        ];
        let filtered = filter_packages(&packages, "", StatusFilter::OUTDATED);
        assert_eq!(
            filtered.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
            ["stale"],
            "a failure is a failed package, not an out-of-date one"
        );
        assert_eq!(
            filter_packages(
                &packages,
                "",
                StatusFilter::with_state(BuildState::Successful)
            )
            .len(),
            2
        );
    }

    /// Out of date combines with the states: a row matches when it meets any
    /// selected condition, so the Packages page gets "failed or out of date".
    #[test]
    fn the_out_of_date_flag_combines_with_the_states() {
        let packages = vec![
            package("stale", BuildState::Successful, true),
            package("current", BuildState::Successful, false),
            package("broken", BuildState::Failed, false),
        ];
        let combined = filter_packages(
            &packages,
            "",
            StatusFilter {
                states: StateSet::from_slice(&[BuildState::Failed]),
                out_of_date: true,
            },
        );
        assert_eq!(
            combined.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
            ["stale", "broken"]
        );
    }

    /// Sorting by status is asking "what needs me", so failures lead and
    /// healthy packages trail — not the numeric order of the enum.
    #[test]
    fn sorting_by_status_puts_problems_first() {
        let mut packages = vec![
            package("ok", BuildState::Successful, false),
            package("broken", BuildState::Failed, false),
            package("running", BuildState::Active, false),
        ];
        sort_packages(
            &mut packages,
            Sort {
                key: SortKey::Status,
                dir: SortDir::Asc,
            },
        );
        let order: Vec<&str> = packages.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(order, ["broken", "running", "ok"]);
    }

    /// An out-of-date package needs attention too, so it sorts ahead of a
    /// current one in the same state.
    #[test]
    fn an_out_of_date_package_outranks_a_current_one() {
        let mut packages = vec![
            package("current", BuildState::Successful, false),
            package("stale", BuildState::Successful, true),
        ];
        sort_packages(
            &mut packages,
            Sort {
                key: SortKey::Status,
                dir: SortDir::Asc,
            },
        );
        assert_eq!(packages[0].name, "stale");
    }

    #[test]
    fn sorting_by_name_is_case_insensitive() {
        let mut packages = vec![
            package("zlib", BuildState::Successful, false),
            package("Apache", BuildState::Successful, false),
        ];
        sort_packages(
            &mut packages,
            Sort {
                key: SortKey::Name,
                dir: SortDir::Asc,
            },
        );
        assert_eq!(packages[0].name, "Apache");
    }

    #[test]
    fn sorting_builds_by_time_orders_by_when_they_started() {
        let mut builds = vec![
            build(1, "a", BuildState::Successful, Some(300)),
            build(2, "b", BuildState::Successful, Some(100)),
            build(3, "c", BuildState::Successful, Some(200)),
        ];
        sort_builds(
            &mut builds,
            Sort {
                key: SortKey::Time,
                dir: SortDir::Desc,
            },
        );
        assert_eq!(
            builds.iter().map(|b| b.number).collect::<Vec<_>>(),
            [1, 3, 2],
            "newest first"
        );
    }

    /// Grouping by package is only useful if each group reads as a history.
    #[test]
    fn builds_of_one_package_stay_newest_first_within_the_group() {
        let mut builds = vec![
            build(1, "pkg", BuildState::Successful, Some(100)),
            build(2, "pkg", BuildState::Successful, Some(300)),
            build(3, "other", BuildState::Successful, Some(200)),
        ];
        sort_builds(
            &mut builds,
            Sort {
                key: SortKey::Name,
                dir: SortDir::Asc,
            },
        );
        assert_eq!(
            builds.iter().map(|b| b.number).collect::<Vec<_>>(),
            [3, 2, 1]
        );
    }

    /// A build still running has no duration, so it sorts last under the
    /// descending order a Duration column opens in, after the longest.
    #[test]
    fn sorting_builds_by_duration_puts_the_longest_first() {
        let mut builds = vec![
            build(1, "a", BuildState::Successful, Some(100)),
            build(2, "b", BuildState::Successful, Some(100)),
            build(3, "c", BuildState::Successful, Some(100)),
        ];
        // #1 runs 100s, #2 runs 300s, #3 is still running (end not recorded).
        builds[0].end_time = Some(200);
        builds[1].end_time = Some(400);
        sort_builds(
            &mut builds,
            Sort {
                key: SortKey::Duration,
                dir: SortDir::Desc,
            },
        );
        assert_eq!(
            builds.iter().map(|b| b.number).collect::<Vec<_>>(),
            [2, 1, 3],
            "longest first, still-running last"
        );
    }

    /// An unrecorded peak groups at the end — it is missing data, not a build
    /// that used no memory — so it lands last under the descending order a
    /// Memory column opens in.
    #[test]
    fn sorting_builds_by_peak_memory_puts_the_highest_first() {
        let mut builds = vec![
            build(1, "a", BuildState::Successful, Some(100)),
            build(2, "b", BuildState::Successful, Some(100)),
            build(3, "c", BuildState::Successful, Some(100)),
        ];
        builds[0].peak_memory = Some(300);
        builds[1].peak_memory = Some(100);
        sort_builds(
            &mut builds,
            Sort {
                key: SortKey::Memory,
                dir: SortDir::Desc,
            },
        );
        assert_eq!(
            builds.iter().map(|b| b.number).collect::<Vec<_>>(),
            [1, 2, 3],
            "highest first, unrecorded last"
        );
    }

    /// Unclaimed builds group at one end: first ascending, last descending --
    /// and within a worker, newest first ascending and oldest first
    /// descending, the exact reverse.
    #[test]
    fn sorting_builds_by_worker_keeps_the_queue_at_one_end() {
        let mut builds = vec![
            build(1, "a", BuildState::Successful, Some(100)),
            build(2, "b", BuildState::Successful, Some(100)),
            build(3, "c", BuildState::Enqueued, None),
            build(4, "d", BuildState::Successful, Some(100)),
        ];
        builds[0].worker_name = Some("Alpha".to_string());
        builds[1].worker_name = Some("beta".to_string());
        builds[3].worker_name = Some("alpha".to_string());
        let numbers = |builds: &[Build]| builds.iter().map(|b| b.number).collect::<Vec<_>>();

        let mut asc = builds.clone();
        sort_builds(
            &mut asc,
            Sort {
                key: SortKey::Worker,
                dir: SortDir::Asc,
            },
        );
        assert_eq!(numbers(&asc), [3, 4, 1, 2]);

        sort_builds(
            &mut builds,
            Sort {
                key: SortKey::Worker,
                dir: SortDir::Desc,
            },
        );
        assert_eq!(numbers(&builds), [2, 1, 4, 3], "the queue goes last");
    }

    /// Clicking the active column reverses it; clicking a new one starts from
    /// the direction that column is usually read in.
    #[test]
    fn clicking_a_column_toggles_or_switches() {
        let by_name = Sort {
            key: SortKey::Name,
            dir: SortDir::Asc,
        };
        assert_eq!(by_name.toggled(SortKey::Name).dir, SortDir::Desc);
        // Time starts newest-first, which is what a log is read as.
        assert_eq!(by_name.toggled(SortKey::Time).dir, SortDir::Desc);
        // So do the other measurements whose interesting end is the top: the
        // longest build and the highest peak first.
        assert_eq!(by_name.toggled(SortKey::Duration).dir, SortDir::Desc);
        assert_eq!(by_name.toggled(SortKey::Memory).dir, SortDir::Desc);
        // Everything else starts A-Z.
        assert_eq!(by_name.toggled(SortKey::Status).dir, SortDir::Asc);
    }

    #[test]
    fn a_status_filter_round_trips_through_its_slug() {
        assert_eq!(StatusFilter::from_slug(""), StatusFilter::ANY);
        assert_eq!(StatusFilter::from_slug("any"), StatusFilter::ANY);
        assert_eq!(StatusFilter::from_slug("outdated"), StatusFilter::OUTDATED);
        for state in [BuildState::Failed, BuildState::Successful] {
            let filter = StatusFilter::with_state(state);
            assert_eq!(StatusFilter::from_slug(&filter.slug()), filter);
        }
        // Sets join with commas, in a stable order.
        let set = StatusFilter::with_states(&[BuildState::Enqueued, BuildState::WaitingForDeps]);
        assert_eq!(set.slug(), "enqueued,waiting-for-deps");
        assert_eq!(StatusFilter::from_slug("enqueued,waiting-for-deps"), set);
        // The flag joins the states.
        let combined = StatusFilter {
            states: StateSet::from_slice(&[BuildState::Failed]),
            out_of_date: true,
        };
        assert_eq!(combined.slug(), "failed,outdated");
        assert_eq!(StatusFilter::from_slug("failed,outdated"), combined);
        // An unrecognised value shows everything rather than nothing, and an
        // unknown slug among known ones is dropped.
        assert_eq!(StatusFilter::from_slug("999"), StatusFilter::ANY);
        assert_eq!(
            StatusFilter::from_slug("failed,bogus"),
            StatusFilter::with_state(BuildState::Failed)
        );
        assert_eq!(StatusFilter::from_slug("bogus"), StatusFilter::ANY);
    }

    #[test]
    fn a_state_set_holds_six_states_and_stays_copy() {
        let mut set = StateSet::empty();
        assert!(set.is_empty());
        assert_eq!(set.len(), 0);
        for state in [
            BuildState::Active,
            BuildState::Successful,
            BuildState::Failed,
            BuildState::Enqueued,
            BuildState::WaitingForDeps,
            BuildState::Publishing,
        ] {
            assert!(!set.contains(state));
            set.insert(state);
            assert!(set.contains(state));
        }
        assert_eq!(set.len(), 6);
        set.remove(BuildState::Failed);
        assert!(!set.contains(BuildState::Failed));
        assert_eq!(set.len(), 5);
        // Empty plus no flag is everything; anything selected is not.
        assert!(StatusFilter::ANY.is_any());
        assert!(StatusFilter::ANY.selected_count() == 0);
        assert!(!StatusFilter::with_state(BuildState::Failed).is_any());
        assert!(!StatusFilter::OUTDATED.is_any());
    }

    #[test]
    fn a_filter_option_is_its_state_label_capitalised() {
        assert_eq!(state_label(BuildState::Active), "Building");
        assert_eq!(state_label(BuildState::WaitingForDeps), "Waiting for deps");
    }

    /// The filter lists its states in the order a status sort shows them.
    #[test]
    fn the_status_filter_lists_states_by_attention() {
        assert_eq!(
            states_by_attention(),
            [
                BuildState::Failed,
                BuildState::Active,
                BuildState::Publishing,
                BuildState::WaitingForDeps,
                BuildState::Enqueued,
                BuildState::Successful,
            ]
        );
    }

    #[test]
    fn the_dropdown_button_names_one_choice_and_counts_the_rest() {
        assert_eq!(filter_summary(StatusFilter::ANY), "Any status");
        assert_eq!(
            filter_summary(StatusFilter::with_state(BuildState::Failed)),
            "Failed"
        );
        assert_eq!(filter_summary(StatusFilter::OUTDATED), "Out of date");
        assert_eq!(
            filter_summary(StatusFilter::with_states(&[
                BuildState::Enqueued,
                BuildState::WaitingForDeps
            ])),
            "2 statuses"
        );
    }
}
