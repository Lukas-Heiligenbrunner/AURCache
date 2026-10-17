use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Duration;

use backon::{FibonacciBuilder, Retryable};

use reqwest::Client;
use url::Url;

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
/// whole version-check pass used to be lost to a single one. Any other error
/// status is an answer, and fails at once.
///
/// The error names the URL without its query. An RPC query lists every
/// package of its batch, which turned one failure into a line thousands of
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

use crate::deps::deps_from_packages;
use crate::model::{
    Dependency, DependencyResolution, Error, Package, PackageResponse, PkgDeps, Resolutions,
};
use crate::repo::{
    OfficialRepos, default_official_mirrorlist_path, default_official_repo_cache_dir,
};
use crate::satisfy::SatisfyIndex;

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
/// The columns of the truth table in [`AurClient::resolve_dependencies`],
/// gathered separately so that combining them is a pure function.
/// [`Evidence::outcome`] is the only encoding of the precedence rule anywhere
/// in the crate: changing what beats what means changing those branches and
/// nothing else.
///
/// Whether a candidate carries the name or merely declares it in `provides`
/// is deliberately not recorded. It decides which candidate *within* a source
/// wins -- [`SatisfyIndex::best_match`] ranks a name above a split package
/// above a `provides` entry -- and nothing beyond that: a name the official
/// repositories only provide still beats one a tracked package carries,
/// because the question this rule answers is which *source* to use.
#[derive(Debug, Default, Clone)]
struct Evidence {
    /// The official repositories publish the name at a version satisfying the
    /// constraint.
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

/// The names no column has settled yet, in declared order.
///
/// Gathering a column costs something -- a disk read, a network round trip --
/// so each is asked only about what is still open. That this is safe is not a
/// second statement of the precedence rule: it *asks* [`Evidence::outcome`],
/// which returns on the first column that answers, so a column that gets
/// skipped provably could not have changed the result. Reordering the
/// gathering below can change how much work happens, never the answer.
fn undecided<'a>(evidence: &[(Dependency<'a>, Evidence)]) -> Vec<&'a str> {
    evidence
        .iter()
        .filter(|(_, found)| found.outcome().is_none())
        .map(|(dep, _)| dep.name)
        .collect()
}

impl AurClient {
    /// Construct a new client from the `AUR_RPC_URL` env var (or default).
    pub fn new() -> Self {
        let rpc_url = std::env::var("AUR_RPC_URL")
            .unwrap_or_else(|_| "https://aur.archlinux.org/rpc/v5".to_string());
        Self::with_urls_and_paths(
            rpc_url,
            default_official_mirrorlist_path(),
            default_official_repo_cache_dir(),
        )
    }

    /// Construct a client with an explicit AUR RPC URL and default filesystem paths.
    pub fn with_urls(aur_url: impl Into<String>) -> Self {
        Self::with_urls_and_paths(
            aur_url,
            default_official_mirrorlist_path(),
            default_official_repo_cache_dir(),
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
        let http = Client::builder()
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
    /// The outcome is a pure function of three independent facts about a name:
    ///
    /// - **tracked** -- `tracked` holds a package claiming the name: AURCache
    ///   has a row for it, or an in-flight add is about to insert one. A
    ///   statement about ownership, not about anything having been built, so a
    ///   package whose last build failed still counts. Version constraints are
    ///   deliberately not consulted (see below).
    /// - **published** -- a repository database lists a package providing the
    ///   name at a version satisfying the constraint: AURCache's own
    ///   repository for the platforms being built, or the cached
    ///   `core`/`extra`/`multilib`. Note this is what the *database* says, not
    ///   that the file is still on disk, and that the official databases are
    ///   fetched for `x86_64` only.
    /// - **in the AUR** -- an AUR package carries the name, or declares it in
    ///   `provides`.
    ///
    /// | official repos | AURCache | in the AUR | outcome |
    /// |---|---|---|---|
    /// | yes | — | — | [`DependencyResolution::Available`] |
    /// | no | tracked | — | [`DependencyResolution::Local`] |
    /// | no | built, no row | — | [`DependencyResolution::Available`] |
    /// | no | no | yes | [`DependencyResolution::Aur`] |
    /// | no | no | no | [`Resolutions::unresolved`] |
    ///
    /// One sentence: **use what the official repositories already hold; if
    /// they do not hold it, use what AURCache already has; only then build
    /// something from the AUR.**
    ///
    /// A source answers whether it carries the name outright or merely
    /// declares it in `provides` -- the two are equivalent here. Which
    /// candidate *within* a source wins is where that distinction lives
    /// ([`SatisfyIndex::best_match`] ranks a name above a split package above
    /// a `provides` entry), and it goes no further: `git-git` declaring
    /// `provides=('git')` does not let a tracked copy of it outrank the `git`
    /// in `extra`, which is the bug this order exists to prevent -- resolving
    /// a name to a package is what makes that package tracked, so one bad
    /// guess would otherwise capture the name for every package added
    /// afterwards.
    ///
    /// The implementation evaluates that table left to right, stopping as soon
    /// as a column settles it. That is an evaluation order, not the
    /// definition: no stage can see what an earlier one concluded, and the
    /// answer for a name does not depend on the other names in the batch.
    ///
    /// # Why the columns are in this order
    ///
    /// Each boundary is load-bearing, so none of them may be swapped for
    /// convenience:
    ///
    /// - **the official repositories before everything.** A name they hold is
    ///   answered by a binary that already exists on every mirror; anything
    ///   else means building a stand-in for it. This is also what stops a
    ///   mistaken provider from entrenching itself, as above.
    /// - **a tracked row before AURCache's own repository.** Both mean
    ///   "AURCache has it", but only a row can be linked to, and the
    ///   dependency edge is what rebuilds dependents when it changes.
    ///   Reaching the repository instead means no row exists -- a deleted
    ///   package whose artifact remains -- and `Available` is correct there,
    ///   since there is nothing to link to.
    /// - **AURCache before the AUR.** A package AURCache tracks is already
    ///   its responsibility, whether or not a build of it has succeeded yet;
    ///   sending the name to the AUR would add a second row for the same
    ///   thing.
    /// - **exact AUR name before `provides`.** Within that last source, a
    ///   package carrying the name outright is a better answer than one that
    ///   merely declares it -- see [`provider_rank`] for how the rest of that
    ///   choice is made.
    ///
    /// Cost happens to agree with the last three, which is why short-circuiting
    /// them is free rather than a compromise: the tracked column is in memory,
    /// AURCache's repository is on disk, and only the AUR columns touch the
    /// network -- the last without a bulk form, so it costs one request per
    /// name left. The first column does not fit that pattern and is asked
    /// about every name, which is the price of it deciding first.
    ///
    /// # Why `tracked` is a parameter
    ///
    /// Not merely because this crate has no database. The set of packages that
    /// can satisfy a dependency is not the set any query returns: during an
    /// add it also includes packages that add has *planned* but not yet
    /// inserted, since the whole graph is resolved before anything is written.
    /// Only the caller knows those. A version of this that fetched its own
    /// rows would still have to be handed the in-flight half, so the parameter
    /// buys correctness at no cost in coupling.
    ///
    /// It also removes an ordering hazard that used to be real: the database
    /// was once a *separate function* callers had to remember to run first,
    /// and calling this one alone reported every package AURCache had already
    /// built as merely available.
    ///
    /// # Which columns check the version constraint
    ///
    /// Only "published". A repository hit *ends* resolution -- the binary
    /// is used as it is, and nothing downstream will ever look at its version
    /// again -- so a repository holding `foo-1.0` must not answer for
    /// `foo>=2.0`. A tracked hit defers instead: the edge it produces records
    /// the constraint, and the build queue re-checks it against each new
    /// build, which is both later and better informed than anything decidable
    /// here. The AUR columns have nothing to check, since the version will be
    /// whatever the build produces.
    ///
    /// `preferred` are package bases the caller would rather have among the
    /// tracked candidates -- in practice, the ones the package being resolved
    /// already depends on. It only reorders column 2: the official
    /// repositories are still asked first, and the AUR still last. Within the
    /// preferred set the usual ranking applies.
    ///
    /// This is what makes a hand-picked provider survive. Re-resolution
    /// recomputes every edge from the declared names, so without it a choice
    /// that beat the ranking once would be undone the next time the package
    /// was updated; with it, the existing edge is the first thing consulted
    /// and only loses if it no longer satisfies the name at all. It also keeps
    /// resolution stable for its own sake: two providers of equal rank stop
    /// trading places between runs.
    pub async fn resolve_dependencies(
        &self,
        deps: &[Dependency<'_>],
        tracked: &SatisfyIndex,
        preferred: &HashSet<String>,
    ) -> Result<Resolutions, Error> {
        // A pkgbase can name the same dependency in `depends` and
        // `makedepends`, and its split packages multiply that again.
        let mut seen = HashSet::new();
        let mut evidence: Vec<(Dependency<'_>, Evidence)> = Vec::new();
        for dep in deps {
            if seen.insert(dep.name) {
                evidence.push((*dep, Evidence::default()));
            }
        }

        // Column 1: what the official repositories publish. From memory --
        // and an error, rather than a "no", while they have never been read.
        for (dep, found) in &mut evidence {
            found.official = self.official.holds(dep.name).await?;
        }

        // Column 2: what AURCache already tracks, `preferred` first. In
        // memory; costs nothing.
        // A row, not a built artifact: the two are kept in step -- deleting a
        // package removes its artifact -- and only a row can carry the
        // dependency edge that rebuilds dependents when it changes.
        for (dep, found) in &mut evidence {
            if found.outcome().is_some() {
                continue;
            }
            found.tracked = tracked
                .best_match(dep.name, |matched| preferred.contains(&matched.pkgbase))
                .or_else(|| tracked.best_match(dep.name, |_| true))
                .map(|matched| matched.pkgbase.clone());
        }

        // Column 3: the AUR, by exact package name. One request for every
        // remaining name, not one per name: `resolve_bases` chunks by URL
        // length, so this is a single call for any realistic dependency list.
        let open = undecided(&evidence);
        if !open.is_empty() {
            let exact_aur_bases = self.resolve_bases(&open).await?;
            for (dep, found) in &mut evidence {
                if found.outcome().is_some() {
                    continue;
                }
                found.aur = exact_aur_bases.get(dep.name).cloned();
            }
        }

        // Column 4: the AUR, by `provides`. No bulk form, so one request each
        // -- which is why it is asked last, of the fewest names.
        for (dep, found) in &mut evidence {
            if found.outcome().is_some() {
                continue;
            }
            found.aur = self.aur_provider_pkgbase(dep.name).await?;
        }

        let mut resolutions = Resolutions::default();
        for (dep, found) in evidence {
            match found.outcome() {
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
        let resp = self.retry_get(url).await?;
        let text = resp.text().await?;
        let response: PackageResponse = serde_json::from_str(&text)?;
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

    /// The AUR package base that declares `dep_name` in its `provides`.
    ///
    /// Ranked through [`SatisfyIndex`] like every other source, so an AUR
    /// package carrying the name outright beats one that merely provides it.
    /// This used to take whichever pkgbase sorted first alphabetically, which
    /// meant a virtual dependency was built from a package chosen by nothing
    /// more than its initial.
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
        // `sort_by` rather than `sort_by_key`: the rank borrows the names it
        // ranks, which a key function may not do.
        packages.sort_by(|a, b| {
            provider_rank(&a.name, &a.package_base, a.num_votes, dep_name).cmp(&provider_rank(
                &b.name,
                &b.package_base,
                b.num_votes,
                dep_name,
            ))
        });
        Ok(packages)
    }
}

/// How good a candidate is at standing in for `dep_name`, lowest first.
///
/// Every candidate provides the name, so there is nothing in the `provides`
/// entries to tell them apart, and something has to choose. This used to be
/// alphabetical order, which is how an instance ended up building `git-git`
/// for `git` and `jpegli-git` for `libjpeg6`: both sort first among their
/// providers, and neither is what anyone meant.
///
/// Three signals, in order:
///
/// 1. **The name itself.** A package called `foo` is a better answer for `foo`
///    than any package merely declaring it.
/// 2. **Not a VCS package.** `-git` and friends build whatever upstream's tip
///    is at the time, so they are a poor way to satisfy someone else's
///    dependency -- they rebuild endlessly and track no release. Someone who
///    wants one adds it directly, which makes it tracked and settles the name
///    before this function is ever reached.
/// 3. **Votes.** Cumulative and integral, where `Popularity` decays and is a
///    float: for `libjpeg6` popularity puts `jpegli-git` (4 votes) ahead of
///    `libjpeg6-turbo` (26), which is exactly backwards.
///
/// Pkgbase breaks the remaining ties, so the answer is at least stable across
/// runs rather than dependent on the order the RPC happened to return.
fn provider_rank<'a>(
    name: &str,
    pkgbase: &'a str,
    votes: u32,
    dep_name: &str,
) -> (u8, u8, std::cmp::Reverse<u32>, &'a str) {
    (
        u8::from(name != dep_name),
        u8::from(is_vcs_pkgbase(pkgbase)),
        std::cmp::Reverse(votes),
        pkgbase,
    )
}

/// Whether a pkgbase names a package that builds from a moving upstream ref.
fn is_vcs_pkgbase(pkgbase: &str) -> bool {
    const VCS_SUFFIXES: [&str; 6] = ["-git", "-svn", "-hg", "-bzr", "-cvs", "-darcs"];
    VCS_SUFFIXES.iter().any(|suffix| pkgbase.ends_with(suffix))
}

#[cfg(test)]
mod provider_rank_tests {
    use super::provider_rank;

    /// Rank candidates the way `aur_provider_pkgbase` does and name the winner.
    fn best<'a>(dep_name: &str, candidates: &[(&'a str, &'a str, u32)]) -> &'a str {
        candidates
            .iter()
            .min_by_key(|(name, pkgbase, votes)| provider_rank(name, pkgbase, *votes, dep_name))
            .map(|(_, pkgbase, _)| *pkgbase)
            .expect("a candidate")
    }

    /// The reported bug: every provider of `git` in the AUR is a VCS package,
    /// and `git-git` wins on alphabetical order alone. (In practice the
    /// repositories answer `git` long before this function is reached -- this
    /// is about which one is chosen when nothing else can answer.)
    #[test]
    fn votes_break_the_tie_that_alphabetical_order_got_wrong() {
        let winner = best(
            "git",
            &[
                ("git-git", "git-git", 3),
                ("git-gl", "git-gl", 2),
                ("git-wd40", "git-wd40", 12),
            ],
        );
        assert_eq!(winner, "git-wd40");
    }

    /// Real `libjpeg6` providers, with their votes. Alphabetical order picks
    /// `jpegli-git`; so does `Popularity`, which is why votes decide.
    #[test]
    fn a_release_package_beats_a_vcs_one_that_sorts_first() {
        let winner = best(
            "libjpeg6",
            &[
                ("jpegli-git", "jpegli-git", 4),
                ("libjpeg6-turbo", "libjpeg6-turbo", 26),
                ("libjpeg6-turbo-bin", "libjpeg6-turbo-bin", 2),
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
            &[
                ("thing-git", "thing-git", 500),
                ("thing-stable", "thing-stable", 1),
            ],
        );
        assert_eq!(winner, "thing-stable");
    }

    /// Carrying the name outright still comes first, VCS or not: a dependency
    /// on `foo-git` means `foo-git`.
    #[test]
    fn the_package_named_for_the_dependency_wins_outright() {
        let winner = best(
            "foo-git",
            &[("other", "other", 900), ("foo-git", "foo-git", 0)],
        );
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

    /// Every combination of the three columns, so the precedence rule is
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

    /// The property the short-circuit in `undecided` relies on: once a column
    /// answers, no later column can change the outcome. If this ever stops
    /// holding, skipping the remaining columns stops being safe.
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
