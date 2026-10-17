//! Detects VCS (`git+...`) sources in a parsed `.SRCINFO` and resolves the
//! commit each currently points to on the remote, without cloning.
//!
//! This lets version checks flag VCS packages (`-git`, `-svn`, `-hg`, `-bzr`
//! style `pkgname`s, though only `git+` sources are currently supported) as
//! out-of-date when upstream has moved, even though the AUR-published
//! `pkgver` for such packages is typically stale (it only reflects when the
//! PKGBUILD itself was last touched, not the live upstream state).
use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Duration;

use alpm_srcinfo::SourceInfoV1;
use alpm_types::Source;
use alpm_types::url::{GitFragment, VcsInfo};
use aurcache_common::worker::JobVcsSource;
use sea_orm::{ActiveValue::Set, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};

use aurcache_db::helpers::builds::latest_successful_build_vcs_sources;
use aurcache_db::helpers::time::now_secs;
use aurcache_db::package_vcs_sources::{self, Entity as PackageVcsSources};

use crate::git::checkout::ls_remote;

/// A single VCS source entry extracted from a package's `.SRCINFO`.
///
/// `source_url` is the raw source string as written in `.SRCINFO` (e.g.
/// `mypkg::git+https://example.com/repo.git#branch=develop`), which serves
/// as a stable, natural per-source key: it already disambiguates multiple
/// source entries within the same package base (that's what the optional
/// `name::` prefix and `#fragment` are for), and two different sources
/// always have different `source_url`s.
#[derive(Clone)]
struct VcsSource {
    source_url: String,
    repo_url: String,
    /// The ref makepkg checks out: a branch or tag name, or `HEAD`.
    git_ref: String,
}

/// What two sources must share for one remote lookup to answer both: the
/// repository and the ref, never the `.SRCINFO` string, which also carries the
/// per-package `name::` prefix.
#[derive(Clone, PartialEq, Eq, Hash)]
struct Remote {
    repo_url: String,
    git_ref: String,
}

impl VcsSource {
    /// Resolve the commit this source currently points to on the remote
    /// (equivalent to `git ls-remote`); does not clone or fetch objects.
    ///
    /// Blocking: performs network I/O and must be run via
    /// `tokio::task::spawn_blocking` from async code.
    fn resolve_commit(&self) -> anyhow::Result<String> {
        ls_remote(&self.repo_url, &self.git_ref)
    }

    fn remote(&self) -> Remote {
        Remote {
            repo_url: self.repo_url.clone(),
            git_ref: self.git_ref.clone(),
        }
    }
}

/// The directory makepkg clones a source into, relative to `SRCDEST`.
///
/// makepkg's own rule (`get_filename`, `util/source.sh`): a `name::` prefix
/// wins outright; otherwise take the URL with its `#fragment` and `?query`
/// stripped, drop a trailing slash, keep the last path component, and cut it at
/// `.git`. So `git+https://github.com/google/fonts.git` lands in `fonts`.
///
/// Derived on the server and sent to the worker rather than worked out at both
/// ends: two implementations of this rule drifting apart would key a commit to
/// a `source_url` nobody compares against, and nothing would look wrong.
fn makepkg_source_dir(filename: Option<&str>, url: &str) -> String {
    if let Some(name) = filename {
        return name.to_string();
    }
    let without_fragment = url.split('#').next().unwrap_or(url);
    let without_query = without_fragment
        .split('?')
        .next()
        .unwrap_or(without_fragment);
    let trimmed = without_query.trim_end_matches('/');
    let last = trimmed.rsplit('/').next().unwrap_or(trimmed);
    last.split(".git").next().unwrap_or(last).to_string()
}

/// The `git+` sources of `sourceinfo`, in the shape a job descriptor carries.
#[must_use]
pub fn job_vcs_sources(sourceinfo: &SourceInfoV1) -> Vec<JobVcsSource> {
    sourceinfo
        .base
        .sources
        .iter()
        .filter_map(|source| {
            let vcs = git_vcs_source_from(source)?;
            let filename = source
                .filename()
                .and_then(|name| name.to_str())
                .map(ToString::to_string);
            Some(JobVcsSource {
                dir: makepkg_source_dir(filename.as_deref(), &vcs.repo_url),
                source_url: vcs.source_url,
                git_ref: vcs.git_ref,
            })
        })
        .collect()
}

/// Extract all *trackable* `git+` VCS sources from `sourceinfo`.
///
/// Only the package base's generic `source` array is considered;
/// architecture-specific overrides (`source_x86_64=(...)` etc.) are not
/// tracked. Sources pinned to an exact `#commit=<sha>` are skipped, since
/// they're immutable by construction and can never go "out of date" — as are
/// `#tag=`/`#branch=` values that are themselves a full commit SHA.
/// Non-git VCS types (`svn+`, `hg+`, `bzr+`, `fossil+`) are not yet
/// supported and are skipped.
fn extract_git_vcs_sources(sourceinfo: &SourceInfoV1) -> Vec<VcsSource> {
    sourceinfo
        .base
        .sources
        .iter()
        .filter_map(git_vcs_source_from)
        .collect()
}

/// Whether a `#branch=`/`#tag=` value is really a pinned commit SHA.
///
/// Some PKGBUILDs write the pin as `#tag=<sha>` (e.g. libgcrypt15's
/// `_tag=6e481d6b…` used as `#tag=${_tag}`): makepkg still checks out that
/// exact commit, so it is immutable like `#commit=` and there is nothing to
/// track. Only full-length SHAs count (SHA-1's 40 or SHA-256's 64 hex
/// digits): a short prefix could also be a genuine branch or tag name.
fn is_commit_sha(name: &str) -> bool {
    (name.len() == 40 || name.len() == 64) && name.bytes().all(|b| b.is_ascii_hexdigit())
}

fn git_vcs_source_from(source: &Source) -> Option<VcsSource> {
    let Source::SourceUrl { source_url, .. } = source else {
        return None;
    };

    let VcsInfo::Git { fragment, .. } = source_url.vcs_info.as_ref()? else {
        return None;
    };

    let git_ref = match fragment {
        // Pinned to an exact commit: never changes, nothing to track.
        Some(GitFragment::Commit(_)) => return None,
        // A tag/branch value that is a full commit SHA pins the same way
        // (`#tag=<sha>`): immutable, nothing to track.
        Some(GitFragment::Branch(name) | GitFragment::Tag(name)) if is_commit_sha(name) => {
            return None;
        }
        Some(GitFragment::Branch(name) | GitFragment::Tag(name)) => name.clone(),
        None => "HEAD".to_string(),
    };

    Some(VcsSource {
        source_url: source.to_string(),
        repo_url: source_url.url.to_string(),
        git_ref,
    })
}

/// Resolve every trackable git-VCS source in `sourceinfo`, upsert their
/// current commit into `package_vcs_sources`, remove rows for sources no
/// longer present (e.g. after a PKGBUILD edit changed/removed a VCS source),
/// and report whether any tracked source's commit moved since last time.
///
/// A newly-seen source (no prior row) also counts as "changed", so the
/// first check after a VCS source is added/discovered is flagged out of
/// date, as a package that has never been built is for pkgver checks.
pub async fn sync_vcs_sources(
    db: &DatabaseConnection,
    package_id: i32,
    sourceinfo: &SourceInfoV1,
    round: &mut RoundCache,
) -> anyhow::Result<VcsSync> {
    let vcs_sources = extract_git_vcs_sources(sourceinfo);

    let existing: HashMap<String, String> = PackageVcsSources::find()
        .filter(package_vcs_sources::Column::PackageId.eq(package_id))
        .all(db)
        .await?
        .into_iter()
        .map(|row| (row.source_url, row.last_commit))
        .collect();

    // What the last successful build was actually made from. This, and not the
    // watermark above, is what "out of date" has to mean: the watermark only
    // moves when the *check* looks, so any build triggered another way -- a
    // manual rebuild, a retry, an unforced update -- left it behind, and the
    // next check re-detected a move it had already built.
    let built = latest_successful_build_vcs_sources(db, package_id).await?;

    let mut changed = false;
    let mut seen_urls: HashSet<String> = HashSet::new();
    let mut upserts = Vec::new();

    for source in vcs_sources {
        let source_url = source.source_url.clone();
        let commit = round.resolve(source).await?;

        seen_urls.insert(source_url.clone());

        if source_moved(&built, &existing, &source_url, &commit) {
            changed = true;
        }

        upserts.push(package_vcs_sources::ActiveModel {
            package_id: Set(package_id),
            source_url: Set(source_url),
            last_commit: Set(commit),
            updated_at: Set(now_secs()),
            ..Default::default()
        });
    }

    // Remove rows for sources no longer present (e.g. patch/PKGBUILD change
    // dropped or renamed a VCS source).
    let stale: Vec<String> = existing
        .keys()
        .filter(|url| !seen_urls.contains(*url))
        .cloned()
        .collect();
    if !stale.is_empty() {
        PackageVcsSources::delete_many()
            .filter(package_vcs_sources::Column::PackageId.eq(package_id))
            .filter(package_vcs_sources::Column::SourceUrl.is_in(stale))
            .exec(db)
            .await?;
    }

    if !upserts.is_empty() {
        PackageVcsSources::insert_many(upserts)
            .on_conflict(
                sea_orm::sea_query::OnConflict::columns([
                    package_vcs_sources::Column::PackageId,
                    package_vcs_sources::Column::SourceUrl,
                ])
                .update_columns([
                    package_vcs_sources::Column::LastCommit,
                    package_vcs_sources::Column::UpdatedAt,
                ])
                .to_owned(),
            )
            .exec(db)
            .await?;
    }

    Ok(if changed {
        VcsSync::Moved
    } else {
        VcsSync::Unchanged
    })
}

/// What [`sync_vcs_sources`] found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VcsSync {
    /// A tracked source's commit moved since it was last built or checked, or
    /// a source was seen for the first time.
    Moved,
    /// Every source is where it was.
    Unchanged,
}

/// Resolve every trackable VCS source of `sourceinfo` to the commit it points
/// at right now.
///
/// One `ls-remote` per source, no clone. Errors are per source rather than
/// fatal: a remote that cannot be reached leaves that source unrecorded, which
/// reads as unknown downstream and costs at most one redundant rebuild -- where
/// failing the caller would cost the build itself.
pub async fn resolve_vcs_commits(sourceinfo: &SourceInfoV1) -> BTreeMap<String, String> {
    let mut resolved = BTreeMap::new();
    for source in extract_git_vcs_sources(sourceinfo) {
        let source_url = source.source_url.clone();
        match tokio::task::spawn_blocking(move || source.resolve_commit()).await {
            Ok(Ok(commit)) => {
                resolved.insert(source_url, commit);
            }
            Ok(Err(e)) => tracing::warn!("could not resolve {source_url}: {e}"),
            Err(e) => tracing::warn!("resolving {source_url} panicked: {e}"),
        }
    }
    resolved
}

/// Whether a package's tracked VCS sources have moved since its last
/// successful build.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourcesMoved {
    /// At least one source is at a commit the last build did not use, or has
    /// no commit recorded for it.
    Moved,
    /// Every source is where the last build left it.
    Unmoved,
    /// The question does not apply: nothing tracked, nothing recorded to
    /// compare against, or no commit could be resolved. Never "up to date".
    Unknown,
}

/// Whether any tracked VCS source has moved since the last successful build,
/// given where they are `now` (from [`resolve_vcs_commits`]).
pub async fn vcs_sources_moved(
    db: &DatabaseConnection,
    package_id: i32,
    now: &BTreeMap<String, String>,
) -> anyhow::Result<SourcesMoved> {
    if now.is_empty() {
        return Ok(SourcesMoved::Unknown);
    }
    let built = latest_successful_build_vcs_sources(db, package_id).await?;
    if built.is_empty() {
        return Ok(SourcesMoved::Unknown);
    }
    // A source with nothing recorded for it is a source we cannot vouch for,
    // so the answer is "moved" rather than a shrug: better a rebuild than a
    // package silently pinned to a commit nobody chose.
    let moved = now
        .iter()
        .any(|(source_url, commit)| built.get(source_url) != Some(commit));
    Ok(if moved {
        SourcesMoved::Moved
    } else {
        SourcesMoved::Unmoved
    })
}

/// Fixed gap between distinct VCS remote lookups in one sweep.
///
/// The version check already resolves remotes sequentially, but back-to-back
/// `ls-remote` calls still arrive as one burst each interval, and a server
/// fronting dozens of them answers some with transient HTTP failures (e.g.
/// 502). A short fixed pause spreads the sweep without needing jitter: there
/// is a single sequential scheduler loop, so nothing else can line up with it.
const VCS_LOOKUP_GAP: Duration = Duration::from_secs(5);

/// Whether a distinct remote lookup waits first: the sweep's first remote goes
/// at once, every later one waits out [`VCS_LOOKUP_GAP`].
fn gap_before_lookup(previous_lookups: u64) -> Option<Duration> {
    if previous_lookups == 0 {
        None
    } else {
        Some(VCS_LOOKUP_GAP)
    }
}

/// Remote commits already resolved during one pass over the packages.
///
/// Several packages can name the same upstream -- `gtk2` and `lib32-gtk2`, and
/// four more pairs on the reference server, 50 tracked sources over 45 distinct
/// URLs -- and each was asking the remote separately. What a remote's ref points
/// at is one fact at one instant, so it is worth asking once; what each package
/// *built* is not, and stays per package.
///
/// Per pass, deliberately, and never held between them: a cache that outlived
/// the round would answer for a moment that has gone, and this is the very
/// thing that decides whether upstream has moved.
#[derive(Default)]
pub struct RoundCache {
    seen: HashMap<Remote, String>,
    lookups: u64,
}

impl RoundCache {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The commit this source points at, asking the remote only the first time
    /// this pass sees it.
    ///
    /// A failure is not cached: the next package naming the same remote should
    /// get its own attempt rather than inheriting a transient error.
    async fn resolve(&mut self, source: VcsSource) -> anyhow::Result<String> {
        let key = source.remote();
        if let Some(commit) = self.seen.get(&key) {
            return Ok(commit.clone());
        }
        if let Some(gap) = gap_before_lookup(self.lookups) {
            tokio::time::sleep(gap).await;
        }
        // Counted before the attempt so a failure still paces the next one:
        // failures are not cached and would otherwise retry back-to-back.
        self.lookups += 1;
        // `resolve_commit` performs blocking network I/O via git2.
        let commit = tokio::task::spawn_blocking(move || source.resolve_commit()).await??;
        self.seen.insert(key, commit.clone());
        Ok(commit)
    }
}

/// Whether a source now at `commit` counts as moved.
///
/// `built` is what the last successful build was made from and is the honest
/// answer; `watermark` is what the version check last saw, used only where the
/// build recorded nothing -- a package built before this was recorded, or a
/// source that could not be resolved at the time. Unknown is never read as
/// unchanged, and nothing is dragged through a rebuild merely for having no
/// record yet.
fn source_moved(
    built: &BTreeMap<String, String>,
    watermark: &HashMap<String, String>,
    source_url: &str,
    commit: &str,
) -> bool {
    match built.get(source_url) {
        Some(built_commit) => built_commit != commit,
        None => watermark.get(source_url).map(String::as_str) != Some(commit),
    }
}

#[cfg(test)]
mod tests {
    use super::{extract_git_vcs_sources, is_commit_sha, source_moved};
    use std::collections::{BTreeMap, HashMap};

    fn map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    /// Distinct VCS lookups are paced: the sweep's first remote goes at once
    /// so a single VCS package pays nothing, and every later one waits out
    /// the fixed gap instead of hitting git servers back-to-back.
    #[test]
    fn vcs_lookups_are_paced_after_the_first() {
        use super::{VCS_LOOKUP_GAP, gap_before_lookup};
        use std::time::Duration;

        assert_eq!(VCS_LOOKUP_GAP, Duration::from_secs(5));
        assert_eq!(gap_before_lookup(0), None);
        assert_eq!(gap_before_lookup(1), Some(Duration::from_secs(5)));
        assert_eq!(gap_before_lookup(40), Some(Duration::from_secs(5)));
    }

    /// makepkg's naming rule, which the worker must not re-derive: the server
    /// sends the directory it produces.
    #[test]
    fn the_source_directory_follows_makepkg() {
        use super::makepkg_source_dir;

        assert_eq!(
            makepkg_source_dir(None, "https://github.com/google/fonts.git"),
            "fonts"
        );
        assert_eq!(
            makepkg_source_dir(None, "https://github.com/google/fonts"),
            "fonts"
        );
        assert_eq!(
            makepkg_source_dir(None, "https://example.test/repo.git#branch=develop"),
            "repo",
            "the fragment is not part of the name"
        );
        assert_eq!(
            makepkg_source_dir(None, "https://example.test/repo.git?signed"),
            "repo",
            "nor is the query"
        );
        assert_eq!(
            makepkg_source_dir(None, "https://example.test/repo/"),
            "repo",
            "a trailing slash does not make the name empty"
        );
        assert_eq!(
            makepkg_source_dir(Some("mypkg"), "https://example.test/repo.git"),
            "mypkg",
            "an explicit `name::` wins outright"
        );
    }

    /// What a build recorded, in the shape the column deserializes to.
    fn built(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    /// What we built is the baseline, even when the check's watermark agrees
    /// with the remote: the watermark only moves when the check looks, so a
    /// build triggered any other way left it behind. Three of
    /// `ttf-google-fonts-git`'s builds in one afternoon were that.
    #[test]
    fn what_was_built_outranks_what_the_check_last_saw() {
        let built = built(&[("src", "new")]);
        let watermark = map(&[("src", "old")]);

        assert!(
            !source_moved(&built, &watermark, "src", "new"),
            "the remote is where we built from, so nothing has moved"
        );
        assert!(
            source_moved(&built, &watermark, "src", "newer"),
            "and when it really has moved, it is still detected"
        );
    }

    /// Without a record the old comparison stands, so the table arriving empty
    /// does not flag every VCS package at once.
    #[test]
    fn with_nothing_recorded_the_watermark_decides() {
        let built = BTreeMap::new();
        let watermark = map(&[("src", "old")]);

        assert!(!source_moved(&built, &watermark, "src", "old"));
        assert!(source_moved(&built, &watermark, "src", "new"));
    }

    /// A source nobody has any record of is new, and new is a reason to build.
    #[test]
    fn an_unknown_source_counts_as_moved() {
        assert!(source_moved(
            &BTreeMap::new(),
            &HashMap::new(),
            "src",
            "whatever"
        ));
    }

    /// One package's sources are keyed separately: a build that recorded only
    /// one of two must not vouch for the other.
    #[test]
    fn each_source_is_judged_on_its_own_record() {
        let built = built(&[("a", "a1")]);
        let watermark = map(&[("a", "a1"), ("b", "b1")]);

        assert!(!source_moved(&built, &watermark, "a", "a1"));
        assert!(
            !source_moved(&built, &watermark, "b", "b1"),
            "b has no build record, so its watermark answers"
        );
        assert!(source_moved(&built, &watermark, "b", "b2"));
    }

    /// Only full-length SHAs pin: a short prefix could be a real branch name.
    #[test]
    fn only_full_length_hex_counts_as_a_pinned_sha() {
        assert!(is_commit_sha("6e481d6bf0a69f8c9bd2866eb491e1e4e9b0717f"));
        assert!(is_commit_sha(&"0123456789abcdef".repeat(4)));
        assert!(!is_commit_sha("6e481d6"), "a short prefix may be a name");
        assert!(!is_commit_sha("v1.5.6"), "a tag name is not a SHA");
        assert!(!is_commit_sha("main"), "a branch name is not a SHA");
        assert!(
            !is_commit_sha("zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz"),
            "40 non-hex chars are not a SHA"
        );
    }

    fn srcinfo_with_source(source: &str) -> alpm_srcinfo::SourceInfoV1 {
        let raw = format!(
            "pkgbase = demo\n\tpkgver = 1.0\n\tpkgrel = 1\n\tarch = x86_64\n\tsource = {source}\n\npkgname = demo\n"
        );
        alpm_srcinfo::SourceInfoV1::from_string(&raw).expect("fixture parses")
    }

    /// libgcrypt15 pins its VCS source as `#tag=<sha>`: makepkg checks out
    /// that exact commit, so it is immutable and there is nothing to track.
    /// Tracking it resolved the default branch tip instead and rebuilt on
    /// every upstream commit without any PKGBUILD change.
    #[test]
    fn a_tag_holding_a_commit_sha_is_not_tracked() {
        let info = srcinfo_with_source(
            "git+https://github.com/gpg/libgcrypt.git#tag=6e481d6bf0a69f8c9bd2866eb491e1e4e9b0717f",
        );
        assert!(
            extract_git_vcs_sources(&info).is_empty(),
            "a #tag=<sha> pin must not be tracked"
        );
    }

    /// The same pin spelled `#branch=<sha>` is a pin all the same.
    #[test]
    fn a_branch_holding_a_commit_sha_is_not_tracked() {
        let info = srcinfo_with_source(
            "git+https://example.test/repo.git#branch=6e481d6bf0a69f8c9bd2866eb491e1e4e9b0717f",
        );
        assert!(
            extract_git_vcs_sources(&info).is_empty(),
            "a #branch=<sha> pin must not be tracked"
        );
    }

    /// Genuine tags and branches are still tracked, and a bare `git+` source
    /// still follows the default branch.
    #[test]
    fn real_tags_branches_and_bare_sources_are_still_tracked() {
        let tagged = srcinfo_with_source("git+https://example.test/repo.git#tag=v1.5.6");
        let tagged = extract_git_vcs_sources(&tagged);
        assert_eq!(tagged.len(), 1);
        assert_eq!(tagged[0].git_ref, "v1.5.6");

        let branched = srcinfo_with_source("git+https://example.test/repo.git#branch=main");
        let branched = extract_git_vcs_sources(&branched);
        assert_eq!(branched.len(), 1);
        assert_eq!(branched[0].git_ref, "main");

        let bare = srcinfo_with_source("git+https://example.test/repo.git");
        let bare = extract_git_vcs_sources(&bare);
        assert_eq!(bare.len(), 1);
        assert_eq!(bare[0].git_ref, "HEAD");
    }
}
