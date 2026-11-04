use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Duration;

use backon::{FibonacciBuilder, Retryable};

use reqwest::Client;
use tracing::warn;
use url::Url;

use crate::deps::deps_from_packages;
use crate::model::{
    Dependency, DependencyResolution, Error, Package, PackageResponse, PkgDeps, Resolutions,
};
use crate::paths::official_repo_cache_dir;
use crate::repo::{OfficialRepos, default_official_mirrorlist_path};
use crate::satisfy::SatisfyIndex;

/// How an outbound request rides out a blip: three attempts, Fibonacci from
/// 500 ms. Defined once so the AUR and the mirrors cannot drift apart.
pub(crate) fn retry_policy() -> FibonacciBuilder {
    FibonacciBuilder::default()
        .with_min_delay(Duration::from_millis(500))
        .with_max_times(3)
}

/// Send a request under [`retry_policy`], keeping only a successful response.
///
/// A 5xx or a 429 is retried like a dropped connection: either is the server
/// having a bad moment, and the AUR's gateway answers 502 often enough that a
/// whole version-check pass would otherwise be lost to a single one. Any other
/// error status is an answer, and fails at once.
///
/// The error names the URL without its query. An RPC query lists every
/// package of its batch, which would make one failure a line thousands of
/// characters long -- in the log and in the activity feed alike -- naming
/// packages that had nothing to do with it.
pub(crate) async fn send_with_retry<F, Fut>(
    mut send: F,
) -> Result<reqwest::Response, reqwest::Error>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<reqwest::Response, reqwest::Error>>,
{
    let attempt = || {
        let response = send();
        async move { response.await?.error_for_status() }
    };
    attempt
        .retry(retry_policy())
        .when(is_transient)
        .await
        .map_err(without_query)
}

/// Whether trying again could get a different answer.
fn is_transient(error: &reqwest::Error) -> bool {
    match error.status() {
        Some(status) => {
            status.is_server_error() || status == reqwest::StatusCode::TOO_MANY_REQUESTS
        }
        None => true,
    }
}

/// The whole of `response`'s body, or how much had arrived when it broke off.
///
/// Read a chunk at a time so that what did arrive can be logged: the AUR's
/// answer to a version check is hundreds of kilobytes, and without it a broken
/// one says only that it broke.
async fn read_body(mut response: reqwest::Response) -> Result<Vec<u8>, Error> {
    let expected = response.content_length();
    let mut body = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => body.extend_from_slice(&chunk),
            Ok(None) => return Ok(body),
            Err(error) => {
                // A chunk's error does not name the request; it gets the URL
                // back without its query, for the reason `send_with_retry`
                // drops it.
                let source = without_query(error.with_url(response.url().clone()));
                warn!(
                    url = ?source.url().map(Url::as_str),
                    received = body.len(),
                    expected,
                    head = %head(&body),
                    tail = %tail(&body),
                    "the AUR's answer broke off"
                );
                return Err(Error::Body {
                    received: body.len(),
                    expected,
                    source,
                });
            }
        }
    }
}

/// How much of a body's start and end to log: enough to tell an error page
/// from JSON, and to see where a broken answer stopped.
const EXCERPT_BYTES: usize = 1024;

/// The start of `body`, for a log line.
fn head(body: &[u8]) -> String {
    String::from_utf8_lossy(&body[..body.len().min(EXCERPT_BYTES)]).into_owned()
}

/// The end of `body`, for a log line.
fn tail(body: &[u8]) -> String {
    String::from_utf8_lossy(&body[body.len().saturating_sub(EXCERPT_BYTES)..]).into_owned()
}

fn without_query(error: reqwest::Error) -> reqwest::Error {
    match error.url().cloned() {
        Some(mut url) => {
            url.set_query(None);
            error.with_url(url)
        }
        None => error,
    }
}

/// Ceiling for a generated RPC URL, under the server's 8 KiB request-line
/// limit with room for the scheme, host and path already in the base.
const MAX_RPC_URL_BYTES: usize = 7_600;

/// Client for the AUR RPC and Arch Linux official package search APIs.
///
/// Handles dependency resolution against AUR packages, official Arch
/// repositories (via a cached local copy of the repo DBs), and packages
/// already present in the local AURCache repository.
/// Neither `Clone` nor `Copy`: one instance per server, shared as an `Arc`.
/// [`OfficialRepos`] owns downloads and the names read from them, and two of
/// those on one cache directory would fetch the same databases twice and hold
/// two answers to the same question.
#[derive(Debug)]
pub struct AurClient {
    pub(crate) http: Client,
    pub(crate) rpc_url: String,
    pub official: OfficialRepos,
}

impl Default for AurClient {
    fn default() -> Self {
        Self::new()
    }
}

/// What each source says about one dependency name.
///
/// Gathered separately so that combining them is a pure function:
/// [`Evidence::outcome`] is the only encoding of the precedence rule in the
/// crate, so changing what beats what means changing those branches and
/// nothing else.
#[derive(Debug, Default, Clone)]
struct Evidence {
    /// The official repositories hold the name, by name or by `provides`.
    official: bool,
    /// The pkgbase of a tracked package claiming the name.
    tracked: Option<String>,
    /// The pkgbase the AUR could build for it.
    aur: Option<String>,
}

impl Evidence {
    /// Combine the evidence. `None` means nothing anywhere provides the name.
    fn outcome(&self) -> Option<DependencyResolution> {
        if self.official {
            return Some(DependencyResolution::Available);
        }
        if let Some(pkgbase) = &self.tracked {
            return Some(DependencyResolution::Local {
                pkgbase: pkgbase.clone(),
            });
        }
        if let Some(pkgbase) = &self.aur {
            return Some(DependencyResolution::Aur {
                pkgbase: pkgbase.clone(),
            });
        }
        None
    }
}

/// One dependency, and what the sources asked so far say about it.
struct Lookup<'a> {
    dep: Dependency<'a>,
    evidence: Evidence,
}

/// The lookups no source has settled yet, in declared order.
///
/// Asking a source costs something -- a disk read, a network round trip -- so
/// each is asked only about what is still open. That is safe because it asks
/// [`Evidence::outcome`], which returns on the first source that answers: a
/// source that gets skipped could not have changed the result.
fn undecided<'l, 'a>(lookups: &'l mut [Lookup<'a>]) -> impl Iterator<Item = &'l mut Lookup<'a>> {
    lookups
        .iter_mut()
        .filter(|lookup| lookup.evidence.outcome().is_none())
}

/// What the server calls itself to the AUR and the mirrors.
pub const USER_AGENT: &str = "AURCache (+https://github.com/Lukas-Heiligenbrunner/AURCache)";

impl AurClient {
    /// Construct a new client from the `AUR_RPC_URL` env var (or default).
    pub fn new() -> Self {
        let rpc_url = std::env::var("AUR_RPC_URL")
            .unwrap_or_else(|_| "https://aur.archlinux.org/rpc/v5".to_string());
        Self::with_urls_and_paths(
            rpc_url,
            default_official_mirrorlist_path(),
            official_repo_cache_dir(),
        )
    }

    /// Construct a client with an explicit AUR RPC URL and default filesystem paths.
    pub fn with_urls(aur_url: impl Into<String>) -> Self {
        Self::with_urls_and_paths(
            aur_url,
            default_official_mirrorlist_path(),
            official_repo_cache_dir(),
        )
    }

    /// Construct a client with full control over the AUR RPC URL and filesystem paths.
    pub fn with_urls_and_paths(
        aur_url: impl Into<String>,
        official_mirrorlist_path: impl Into<PathBuf>,
        official_repo_cache_dir: impl Into<PathBuf>,
    ) -> Self {
        // One `reqwest::Client` for the AUR and the mirrors alike: it is a
        // handle to a connection pool, so cloning shares the pool rather than
        // opening a second one.
        //
        // Bounded, or a blackholed network hangs every caller forever — most
        // sharply the dependency backfill migration, which runs at startup
        // before anything else can time it out. These are metadata RPCs and
        // mirrorlist reads, never bulk transfers, so a minute total is a
        // hung socket, not a slow server.
        //
        // Named: the AUR answers a request without a User-Agent with a bot
        // challenge page rather than JSON, which fails every lookup.
        let http = Client::builder()
            .user_agent(USER_AGENT)
            .connect_timeout(std::time::Duration::from_secs(15))
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .unwrap_or_else(|_| Client::new());
        Self {
            official: OfficialRepos::new(
                http.clone(),
                official_mirrorlist_path.into(),
                official_repo_cache_dir.into(),
            ),
            http,
            rpc_url: aur_url.into(),
        }
    }

    fn rpc_info_url(&self, args: &[&str]) -> Result<Url, Error> {
        let mut url = Url::parse(&format!("{}/info", self.rpc_url))?;
        for arg in args {
            url.query_pairs_mut().append_pair("arg[]", arg);
        }
        Ok(url)
    }

    /// Split an `/info` query across as many URLs as the length limit requires.
    ///
    /// The RPC takes one `arg[]` per package on a GET, and the server rejects a
    /// request line over 8 KiB with `414 Request-URI Too Large` — measured:
    /// 8185 bytes answers, 8313 does not. At roughly 25 bytes per package that
    /// caps a single request near 300 packages, which a repository of any size
    /// passes.
    ///
    /// Getting this wrong is not a partial failure: the version-check pass
    /// propagates the error, so one oversized request stops *every* package
    /// from being checked, on every pass, until the package count drops.
    ///
    /// Measured against the encoded URL rather than a package count, because
    /// names are percent-encoded — `aewm++` is four bytes longer than it looks.
    fn rpc_info_urls(&self, args: &[&str]) -> Result<Vec<Url>, Error> {
        let base = format!("{}/info", self.rpc_url);
        let mut urls = Vec::new();
        let mut current = Url::parse(&base)?;
        let mut in_current = 0usize;

        for arg in args {
            let mut candidate = current.clone();
            candidate.query_pairs_mut().append_pair("arg[]", arg);

            if in_current > 0 && candidate.as_str().len() > MAX_RPC_URL_BYTES {
                urls.push(current);
                current = Url::parse(&base)?;
                current.query_pairs_mut().append_pair("arg[]", arg);
                in_current = 1;
            } else {
                // A single argument that alone exceeds the budget still goes
                // out on its own: a doomed request beats silently dropping the
                // package from version checking.
                current = candidate;
                in_current += 1;
            }
        }

        if in_current > 0 {
            urls.push(current);
        }
        Ok(urls)
    }

    /// Resolve a list of package names to their pkgbase names via the AUR RPC.
    pub async fn resolve_bases(&self, names: &[&str]) -> Result<HashMap<String, String>, Error> {
        Ok(self
            .multi_info_of(names)
            .await?
            .into_iter()
            .map(|pkg| (pkg.name, pkg.package_base))
            .collect())
    }

    /// Fetch the dependency lists for a pkgbase via the AUR RPC.
    pub async fn deps_of(&self, pkgbase: &str) -> Result<PkgDeps, Error> {
        let packages = self.rpc_request(&[pkgbase]).await?;
        Ok(deps_from_packages(&packages))
    }

    /// Fetch metadata for a single AUR package by name, returning `None` if not found.
    pub async fn info_of(&self, name: &str) -> Result<Option<Package>, Error> {
        let packages = self.rpc_fetch(self.rpc_info_url(&[name])?).await?;
        Ok(packages.into_iter().next())
    }

    /// Fetch metadata for several AUR packages at once.
    ///
    /// A name the AUR does not know is absent from the result, not an error:
    /// the version check asks about every tracked package together, and one
    /// that left the AUR -- or all of them -- must not cost the others their
    /// check.
    ///
    /// Chunked across URLs by [`Self::rpc_info_urls`], since a whole
    /// dependency list or repository can outgrow the server's URL limit. Only
    /// a request that fails outright aborts.
    pub async fn multi_info_of(&self, names: &[&str]) -> Result<Vec<Package>, Error> {
        let mut packages = Vec::new();
        for url in self.rpc_info_urls(names)? {
            packages.extend(self.rpc_fetch(url).await?);
        }
        Ok(packages)
    }

    /// Search AUR packages by name or description.
    pub async fn search_by_name(&self, query: &str) -> Result<Vec<Package>, Error> {
        let url = self.rpc_search_url(query, "name-desc")?;
        self.rpc_fetch(url).await
    }

    /// Resolve dependencies to what should happen about each of them.
    ///
    /// Each name goes to the first source that answers for it, by name or by
    /// `provides`:
    ///
    /// 1. the official repositories hold it: [`DependencyResolution::Available`];
    /// 2. a package in `tracked` claims it: [`DependencyResolution::Local`];
    /// 3. an AUR package carries the name, or else declares it in `provides`:
    ///    [`DependencyResolution::Aur`];
    /// 4. nothing does: the name is listed in [`Resolutions::unresolved`].
    ///
    /// Use what the official repositories hold; failing that, what AURCache
    /// already has; only then build something from the AUR. Carrying a name
    /// rather than providing it only ranks candidates *within* a source
    /// ([`SatisfyIndex::best_match`], [`ProviderRank`]): `git-git` declaring
    /// `provides=('git')` never outranks the `git` in `extra`. Resolving a
    /// name to a package makes that package tracked, so one bad guess would
    /// otherwise capture the name for every package added afterwards.
    ///
    /// No version constraint is checked here. A tracked package answers
    /// whether or not it has built: the edge records the constraint, and the
    /// build queue checks it against each build.
    ///
    /// `tracked` is every package that may satisfy a dependency: AURCache's
    /// rows and, during an add, the packages it has planned but not inserted
    /// yet, which only the caller knows. `preferred` are package bases to
    /// favour among them -- in practice the ones the package being resolved
    /// already depends on, so that a provider picked by hand survives
    /// re-resolution and two equal providers do not trade places between runs.
    ///
    /// # Errors
    ///
    /// [`Error::OfficialReposUnread`] while the official databases have never
    /// been read, rather than sending every name they hold to the AUR; and any
    /// AUR request that fails.
    pub async fn resolve_dependencies(
        &self,
        deps: &[Dependency<'_>],
        tracked: &SatisfyIndex,
        preferred: &HashSet<String>,
    ) -> Result<Resolutions, Error> {
        // A pkgbase can name the same dependency in `depends` and
        // `makedepends`, and its split packages multiply that again.
        let mut seen = HashSet::new();
        let mut lookups: Vec<Lookup<'_>> = deps
            .iter()
            .filter(|dep| seen.insert(dep.name))
            .map(|&dep| Lookup {
                dep,
                evidence: Evidence::default(),
            })
            .collect();

        // The official repositories, from memory, about every name.
        for lookup in &mut lookups {
            lookup.evidence.official = self.official.holds(lookup.dep.name).await?;
        }

        // What AURCache already tracks, `preferred` first. In memory.
        for lookup in undecided(&mut lookups) {
            let name = lookup.dep.name;
            lookup.evidence.tracked = tracked
                .best_match(name, |matched| preferred.contains(&matched.pkgbase))
                .or_else(|| tracked.best_match(name, |_| true))
                .map(|matched| matched.pkgbase.clone());
        }

        // The AUR, by exact package name: one call for every remaining name,
        // which `resolve_bases` chunks by URL length.
        let open: Vec<&str> = undecided(&mut lookups).map(|l| l.dep.name).collect();
        if !open.is_empty() {
            let exact_aur_bases = self.resolve_bases(&open).await?;
            for lookup in undecided(&mut lookups) {
                lookup.evidence.aur = exact_aur_bases.get(lookup.dep.name).cloned();
            }
        }

        // The AUR, by `provides`. No bulk form, so one request each -- which
        // is why it is asked last, of the fewest names.
        for lookup in undecided(&mut lookups) {
            lookup.evidence.aur = self.aur_provider_pkgbase(lookup.dep.name).await?;
        }

        let mut resolutions = Resolutions::default();
        for Lookup { dep, evidence } in lookups {
            match evidence.outcome() {
                Some(resolution) => {
                    resolutions.found.insert(dep.name.to_string(), resolution);
                }
                None => resolutions.unresolved.push(dep.name.to_string()),
            }
        }
        Ok(resolutions)
    }

    fn rpc_search_url(&self, query: &str, by: &str) -> Result<Url, Error> {
        let mut url = Url::parse(&format!("{}/search", self.rpc_url))?;
        url.path_segments_mut()
            .map_err(|_| Error::Rpc("Invalid RPC search URL".to_string()))?
            .push(query);
        url.query_pairs_mut().append_pair("by", by);
        Ok(url)
    }

    async fn rpc_fetch(&self, url: Url) -> Result<Vec<Package>, Error> {
        let body = read_body(self.retry_get(url).await?).await?;
        let response: PackageResponse = serde_json::from_slice(&body).map_err(|error| {
            warn!(
                %error,
                bytes = body.len(),
                head = %head(&body),
                tail = %tail(&body),
                "the AUR's answer is not the JSON expected"
            );
            Error::Json(error)
        })?;
        if response.response_type == "error" {
            return Err(Error::Rpc(
                response
                    .error
                    .unwrap_or_else(|| "AUR RPC returned error".to_string()),
            ));
        }
        Ok(response.results)
    }

    async fn rpc_request(&self, args: &[&str]) -> Result<Vec<Package>, Error> {
        let packages = self.rpc_fetch(self.rpc_info_url(args)?).await?;
        if packages.is_empty() {
            return Err(Error::Rpc("package not found via RPC".into()));
        }
        Ok(packages)
    }

    /// Perform an HTTP GET through [`send_with_retry`].
    pub(crate) async fn retry_get<U: reqwest::IntoUrl + Clone>(
        &self,
        url: U,
    ) -> Result<reqwest::Response, Error> {
        send_with_retry(|| self.http.get(url.clone()).send())
            .await
            .map_err(Error::Http)
    }

    /// The best AUR package base declaring `dep_name` in its `provides`, as
    /// [`ProviderRank`] orders them.
    async fn aur_provider_pkgbase(&self, dep_name: &str) -> Result<Option<String>, Error> {
        Ok(self
            .aur_providers(dep_name)
            .await?
            .into_iter()
            .next()
            .map(|package| package.package_base))
    }

    /// Every AUR package answering to `dep_name`, best first.
    ///
    /// Resolution takes the head of this list; offering a dependency edge
    /// somewhere else to point needs the rest of it, ranked the same way, so
    /// that the option a person is shown first is the one resolution would
    /// have picked on its own.
    pub async fn aur_providers(&self, dep_name: &str) -> Result<Vec<Package>, Error> {
        let mut packages = self
            .rpc_fetch(self.rpc_search_url(dep_name, "provides")?)
            .await?;

        // The query is the evidence: every result provides `dep_name` by
        // construction, since the server filtered on exactly that. The RPC's
        // *search* response carries only identity fields -- no `provides`,
        // unlike `info` -- so re-deriving the claim from the response body
        // would find nothing and quietly resolve every virtual dependency to
        // nothing at all.
        packages.sort_by(|a, b| ProviderRank::of(a, dep_name).cmp(&ProviderRank::of(b, dep_name)));
        Ok(packages)
    }
}

/// How good a candidate is at standing in for a dependency, best first.
///
/// Every candidate provides the name, so there is nothing in the `provides`
/// entries to tell them apart, and something has to choose. The fields are
/// compared in order:
///
/// 1. **The name itself.** A package called `foo` is a better answer for `foo`
///    than any package merely declaring it.
/// 2. **Not a VCS package.** `-git` and friends build whatever upstream's tip
///    is at the time, so they are a poor way to satisfy someone else's
///    dependency -- they rebuild endlessly and track no release. Someone who
///    wants one adds it directly, which makes it tracked and settles the name
///    before this is ever reached.
/// 3. **Votes.** Cumulative and integral, where `Popularity` decays and is a
///    float: for `libjpeg6` popularity puts `jpegli-git` (4 votes) ahead of
///    `libjpeg6-turbo` (26), which is exactly backwards.
/// 4. **The pkgbase**, so the answer is stable across runs rather than
///    dependent on the order the RPC happened to return.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct ProviderRank<'a> {
    named_otherwise: bool,
    vcs: bool,
    votes: Reverse<u32>,
    pkgbase: &'a str,
}

impl<'a> ProviderRank<'a> {
    fn of(package: &'a Package, dep_name: &str) -> Self {
        Self {
            named_otherwise: package.name != dep_name,
            vcs: is_vcs_pkgbase(&package.package_base),
            votes: Reverse(package.num_votes),
            pkgbase: &package.package_base,
        }
    }
}

/// Whether a pkgbase names a package that builds from a moving upstream ref.
fn is_vcs_pkgbase(pkgbase: &str) -> bool {
    const VCS_SUFFIXES: [&str; 6] = ["-git", "-svn", "-hg", "-bzr", "-cvs", "-darcs"];
    VCS_SUFFIXES.iter().any(|suffix| pkgbase.ends_with(suffix))
}

#[cfg(test)]
mod provider_rank_tests {
    use super::ProviderRank;
    use crate::model::Package;

    /// An AUR package whose pkgbase is its name.
    fn package(name: &str, votes: u32) -> Package {
        Package {
            name: name.to_string(),
            package_base: name.to_string(),
            num_votes: votes,
            ..Package::default()
        }
    }

    /// The pkgbase that ranks first among `candidates` for `dep_name`.
    fn best(dep_name: &str, candidates: &[Package]) -> String {
        candidates
            .iter()
            .min_by_key(|package| ProviderRank::of(package, dep_name))
            .map(|package| package.package_base.clone())
            .expect("a candidate")
    }

    /// Every provider of `git` in the AUR is a VCS package, so votes decide,
    /// not alphabetical order. (In practice the repositories answer `git`
    /// long before this is reached -- this is about which one is chosen when
    /// nothing else can answer.)
    #[test]
    fn votes_break_a_tie_between_vcs_packages() {
        let winner = best(
            "git",
            &[
                package("git-git", 3),
                package("git-gl", 2),
                package("git-wd40", 12),
            ],
        );
        assert_eq!(winner, "git-wd40");
    }

    /// Real `libjpeg6` providers, with their votes. Alphabetical order would
    /// pick `jpegli-git`; so would `Popularity`, which is why votes decide.
    #[test]
    fn a_release_package_beats_a_vcs_one_that_sorts_first() {
        let winner = best(
            "libjpeg6",
            &[
                package("jpegli-git", 4),
                package("libjpeg6-turbo", 26),
                package("libjpeg6-turbo-bin", 2),
            ],
        );
        assert_eq!(winner, "libjpeg6-turbo");
    }

    /// A VCS package loses even to a much less popular release package: the
    /// objection to it is that it tracks no release, not that it is unloved.
    #[test]
    fn a_vcs_package_loses_on_kind_not_on_votes() {
        let winner = best(
            "thing",
            &[package("thing-git", 500), package("thing-stable", 1)],
        );
        assert_eq!(winner, "thing-stable");
    }

    /// Carrying the name outright still comes first, VCS or not: a dependency
    /// on `foo-git` means `foo-git`.
    #[test]
    fn the_package_named_for_the_dependency_wins_outright() {
        let winner = best("foo-git", &[package("other", 900), package("foo-git", 0)]);
        assert_eq!(winner, "foo-git");
    }
}

#[cfg(test)]
mod evidence_tests {
    use super::Evidence;
    use crate::model::DependencyResolution;

    fn evidence(official: bool, tracked: bool, aur: bool) -> Evidence {
        Evidence {
            official,
            tracked: tracked.then(|| "tracked-base".to_string()),
            aur: aur.then(|| "aur-base".to_string()),
        }
    }

    /// Every combination of the three sources, so the precedence rule is
    /// pinned by a test rather than by the order of the statements that happen
    /// to gather it. No repository, no database, no network.
    #[test]
    fn the_truth_table_holds_for_every_combination() {
        let local = Some(DependencyResolution::Local {
            pkgbase: "tracked-base".to_string(),
        });
        let available = Some(DependencyResolution::Available);
        let aur = Some(DependencyResolution::Aur {
            pkgbase: "aur-base".to_string(),
        });

        // (official, tracked, aur) -> outcome
        let cases = [
            ((true, true, true), available.clone()),
            ((true, true, false), available.clone()),
            ((true, false, true), available.clone()),
            ((true, false, false), available),
            ((false, true, true), local.clone()),
            ((false, true, false), local),
            ((false, false, true), aur),
            ((false, false, false), None),
        ];

        for ((official, tracked, in_aur), expected) in cases {
            assert_eq!(
                evidence(official, tracked, in_aur).outcome(),
                expected,
                "official={official} tracked={tracked} aur={in_aur}"
            );
        }
    }

    /// The bug this order exists to prevent.
    ///
    /// `git-git` declares `provides=('git')`, and the `git` in `extra` carries
    /// the name. Resolving a name to a package is what makes that package
    /// tracked, so an instance that resolved `git` to `git-git` once -- which
    /// took only a moment where the official databases could not be read --
    /// kept resolving it that way afterwards, for every package added since.
    /// Reported as `lib32-libidn11` depending on `git-git`.
    #[test]
    fn a_tracked_package_does_not_capture_a_name_the_official_repositories_hold() {
        assert_eq!(
            evidence(true, true, true).outcome(),
            Some(DependencyResolution::Available)
        );
    }

    /// The property the short-circuit in `undecided` relies on: once a source
    /// answers, no later source can change the outcome. If this ever stops
    /// holding, skipping the remaining sources stops being safe.
    #[test]
    fn later_columns_cannot_override_an_earlier_answer() {
        for tracked in [false, true] {
            for in_aur in [false, true] {
                assert_eq!(
                    evidence(true, tracked, in_aur).outcome(),
                    evidence(true, false, false).outcome(),
                    "an official package must decide regardless of later columns"
                );
            }
        }
        for in_aur in [false, true] {
            assert_eq!(
                evidence(false, true, in_aur).outcome(),
                evidence(false, true, false).outcome(),
                "a tracked package must decide regardless of the AUR"
            );
        }
    }
}

#[cfg(test)]
mod url_chunking_tests {
    use super::{AurClient, MAX_RPC_URL_BYTES};

    fn client() -> AurClient {
        AurClient::with_urls("https://aur.archlinux.org/rpc/v5")
    }

    /// A handful of packages is one request, as before.
    #[test]
    fn a_small_query_is_a_single_request() {
        let names = ["hello", "yay", "paru"];
        let urls = client().rpc_info_urls(&names).expect("urls");
        assert_eq!(urls.len(), 1);
        assert_eq!(urls[0].query_pairs().count(), 3);
    }

    /// The case that broke: a repository with enough packages to outgrow the
    /// server's 8 KiB request line. Measured against the real service — 8185
    /// bytes answers, 8313 returns 414.
    #[test]
    fn a_large_query_is_split_and_every_part_fits() {
        let names: Vec<String> = (0..1000)
            .map(|i| format!("some-package-name-{i}"))
            .collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();

        let urls = client().rpc_info_urls(&refs).expect("urls");
        assert!(urls.len() > 1, "1000 packages should not be one request");

        for url in &urls {
            assert!(
                url.as_str().len() <= MAX_RPC_URL_BYTES,
                "chunk of {} bytes exceeds the budget",
                url.as_str().len()
            );
        }

        // Every package is asked about exactly once — chunking must not drop
        // or duplicate any, which would silently stop them being checked.
        let asked: Vec<String> = urls
            .iter()
            .flat_map(|u| {
                u.query_pairs()
                    .map(|(_, v)| v.to_string())
                    .collect::<Vec<_>>()
            })
            .collect();
        assert_eq!(asked, names);
    }

    /// Names are percent-encoded, so a count-based split would misjudge the
    /// length. 187 AUR packages contain `+`, which triples in the URL.
    #[test]
    fn encoded_names_are_measured_at_their_encoded_length() {
        let names: Vec<String> = (0..1000).map(|i| format!("aewm++{i}+plus+name")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();

        for url in client().rpc_info_urls(&refs).expect("urls") {
            assert!(
                url.as_str().len() <= MAX_RPC_URL_BYTES,
                "encoded chunk of {} bytes exceeds the budget",
                url.as_str().len()
            );
        }
    }

    /// No packages means no requests, rather than one pointless empty query.
    #[test]
    fn an_empty_query_makes_no_requests() {
        assert!(client().rpc_info_urls(&[]).expect("urls").is_empty());
    }
}
