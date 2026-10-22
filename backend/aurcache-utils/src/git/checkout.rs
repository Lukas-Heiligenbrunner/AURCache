use anyhow::{anyhow, bail};
use git2::{Direction, Oid, Repository};
use std::path::Path;

/// Resolve `git_ref` against the *fetched remote* state, falling back to a
/// local resolve.
///
/// `fetch` only advances remote-tracking refs (`refs/remotes/origin/*`). The
/// local branch — and therefore local `HEAD` — stays wherever the initial clone
/// left it. Resolving locally would pin a re-used checkout to its first-cloned
/// commit forever: upstream could publish any number of new versions and every
/// subsequent resolve would still return the original commit and the original
/// `.SRCINFO`.
///
/// AUR sources are resolved with `git_ref = "HEAD"`, so they hit exactly that
/// path; the symptom is AURCache reporting an available update (that check uses
/// the live AUR RPC) while the update itself refuses with "Latest build is
/// already up to date", reading the stale pkgver off the frozen checkout.
///
/// Tags and raw commit SHAs have no remote-tracking equivalent, so a local
/// resolve remains the fallback.
fn resolve_remote_ref<'a>(
    repo: &'a Repository,
    git_ref: &str,
) -> anyhow::Result<(git2::Object<'a>, Option<git2::Reference<'a>>)> {
    let candidates: Vec<String> = if git_ref == "HEAD" {
        // The clone left HEAD on whatever the remote's default branch is, and
        // we keep it there (see `resolve_and_checkout`), so ask the checkout
        // rather than guessing at `master`/`main` — plenty of repos use
        // neither. `origin/HEAD` is the fallback for a checkout that is
        // detached because it previously resolved a tag or pinned SHA.
        head_branch(repo)
            .map(|branch| format!("refs/remotes/origin/{}", branch.shorthand))
            .into_iter()
            .chain(["refs/remotes/origin/HEAD".to_string()])
            .collect()
    } else {
        // A caller-supplied `origin/foo` is already remote-tracking.
        vec![
            format!("refs/remotes/origin/{git_ref}"),
            git_ref.to_string(),
        ]
    };

    for candidate in &candidates {
        if let Ok(resolved) = repo.revparse_ext(candidate) {
            return Ok(resolved);
        }
    }
    repo.revparse_ext(git_ref)
        .map_err(|e| anyhow!("could not resolve git ref '{git_ref}': {e}"))
}

/// A clone that succeeded and brought back nothing.
///
/// Its own type rather than a message because a caller has to *recognise* it:
/// the AUR answers a clone for an unknown package with an empty repository, so
/// this is how a misspelled package name arrives, and
/// `aurcache_utils::snapshot` turns it into a sentence about the AUR. Matching
/// on the wording of an error would make that a coincidence rather than a
/// contract.
#[derive(Debug)]
pub struct EmptyRepository {
    pub url: String,
}

impl std::fmt::Display for EmptyRepository {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the git repository at {} is empty (it has no commits)",
            self.url
        )
    }
}

impl std::error::Error for EmptyRepository {}

/// A local branch, by both of its names.
struct Branch {
    /// The full ref name, `refs/heads/main`.
    name: String,
    /// `main`.
    shorthand: String,
}

/// The branch HEAD is on, or `None` when the checkout is detached.
fn head_branch(repo: &Repository) -> Option<Branch> {
    let head = repo.head().ok()?;
    if !head.is_branch() {
        return None;
    }
    Some(Branch {
        name: head.name().ok()?.to_string(),
        shorthand: head.shorthand().ok()?.to_string(),
    })
}

/// Resolve `git_ref` to an object in `repo` and checkout its tree, updating HEAD.
/// Shared between a fresh clone and a re-used, freshly-fetched repo.
fn resolve_and_checkout(repo: &Repository, git_ref: &str) -> anyhow::Result<Oid> {
    // Resolve the ref to an object
    let (object, reference) = resolve_remote_ref(repo, git_ref)?;

    // Checkout the tree (updates working directory), forcing it to match
    // exactly in case a previous checkout left local modifications (e.g. a
    // dirty working dir from an interrupted build).
    let mut checkout_builder = git2::build::CheckoutBuilder::new();
    checkout_builder.force();
    repo.checkout_tree(&object, Some(&mut checkout_builder))?;

    // A remote-tracking ref must not become HEAD — git would treat the checkout
    // as being "on" origin/master. Three cases, in order:
    let local_ref_name = reference
        .as_ref()
        .and_then(|r| r.name().ok())
        .filter(|name| !name.starts_with("refs/remotes/"))
        .map(ToString::to_string);
    // Only when following the default branch: advancing HEAD's branch to some
    // *other* ref's commit would silently rewrite it.
    let tracked_branch = if git_ref == "HEAD" {
        head_branch(repo)
    } else {
        None
    };

    match (local_ref_name, tracked_branch) {
        // Resolved to a local branch or tag: point HEAD at it directly.
        (Some(name), _) => repo.set_head(&name)?,
        // Following the default branch: fast-forward it to what we fetched and
        // stay on it. `fetch` only moves `refs/remotes/*`, so without this the
        // branch would lag forever — and keeping HEAD on a branch is what lets
        // the next call read the default branch's name back off the checkout
        // instead of guessing it.
        (None, Some(branch)) => {
            repo.reference(&branch.name, object.id(), true, "follow upstream")?;
            repo.set_head(&branch.name)?;
        }
        // A tag or pinned SHA: nothing to track, so detach. This is a
        // read-only source cache; nothing commits here.
        (None, None) => repo.set_head_detached(object.id())?,
    }
    Ok(object.id())
}

/// Open the repo at `path` if it already exists, otherwise clone `git_repo` into it.
/// Either way, fetch the latest state of `git_ref` from the remote and check it out,
/// returning the resolved commit id.
///
/// This allows repeated calls (e.g. from a persistent on-disk cache) to reuse the
/// existing clone and only transfer new objects via `fetch`, instead of re-cloning
/// the whole repository every time.
pub fn checkout_or_fetch_repo_ref(
    git_repo: &str,
    git_ref: &str,
    path: &Path,
) -> anyhow::Result<Oid> {
    let repo = if path.join(".git").exists() {
        let repo = Repository::open(path)?;
        // Make sure `origin` still points at the expected URL (it may have
        // changed if the package's source spec was edited).
        {
            let mut remote = match repo.find_remote("origin") {
                Ok(remote) => remote,
                Err(_) => repo.remote("origin", git_repo)?,
            };
            if remote.url().ok() != Some(git_repo) {
                repo.remote_set_url("origin", git_repo)?;
                remote = repo.find_remote("origin")?;
            }
            // Fetch using the remote's default refspecs (branches/tags) so that
            // `git_ref` can later be resolved by `revparse_ext`. Also try to fetch
            // `git_ref` directly, to cover the case of a raw commit SHA that isn't
            // reachable from any branch/tag tip.
            remote.fetch(&[] as &[&str], None, None)?;
            let _ = remote.fetch(&[git_ref], None, None);
        }
        repo
    } else {
        std::fs::create_dir_all(path)?;
        Repository::clone(git_repo, path)?
    };

    // A repository with no commits cannot have the ref resolved out of it, and
    // the reason is worth saying plainly: `revparse` would otherwise report a
    // missing `HEAD`, which reads as a broken ref rather than as an empty
    // remote. The AUR serves exactly this for a package name it does not know
    // -- the clone succeeds and brings back nothing -- so this is the shape a
    // misspelled package arrives in.
    if repo.is_empty()? {
        bail!(EmptyRepository {
            url: git_repo.to_string()
        });
    }

    resolve_and_checkout(&repo, git_ref)
}

/// The remote refs a `git_ref` may be advertised under: the exact name,
/// then `refs/heads/<name>`, then `refs/tags/<name>`.
///
/// There is deliberately no `HEAD` fallback. `HEAD` is only a candidate when
/// it was asked for: an unknown ref used to fall through to the remote's
/// `HEAD`, so a mistyped branch/tag — or a `#tag=<sha>` pin — silently
/// tracked the default branch tip and rebuilt on every upstream commit.
fn ls_remote_candidates(git_ref: &str) -> Vec<String> {
    if git_ref == "HEAD" {
        return vec!["HEAD".to_string()];
    }
    vec![
        git_ref.to_string(),
        format!("refs/heads/{git_ref}"),
        format!("refs/tags/{git_ref}"),
    ]
}

/// Resolve the commit that `git_ref` currently points to on the remote
/// `git_repo`, without cloning or fetching any objects (equivalent to
/// `git ls-remote <repo> <ref>`).
///
/// Uses an in-memory (non-cloned) `git2` remote connection. `git_ref` may be
/// a branch name, tag name, or `HEAD`; ambiguous short names are resolved the
/// same way the remote's advertised ref list would allow (exact ref name,
/// then `refs/heads/<name>`, then `refs/tags/<name>`).
pub fn ls_remote(git_repo: &str, git_ref: &str) -> anyhow::Result<String> {
    // A throwaway repository is required to create a remote in git2, even
    // for a purely in-memory listing; it performs no disk I/O for the actual
    // remote sources.
    let dir = tempfile::tempdir()?;
    let repo = Repository::init_bare(dir.path())?;
    let mut remote = repo.remote_anonymous(git_repo)?;
    remote.connect(Direction::Fetch)?;

    let heads = remote.list()?;
    let candidates = ls_remote_candidates(git_ref);

    let result = candidates
        .iter()
        .find_map(|candidate| heads.iter().find(|h| h.name() == candidate))
        .map(|head| head.oid().to_string());

    remote.disconnect()?;
    drop(remote);
    drop(repo);
    dir.close()?;

    result.ok_or_else(|| anyhow!("Ref '{git_ref}' not found on remote '{git_repo}'"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use git2::Signature;

    fn commit(repo: &Repository, pkgver: &str) {
        let workdir = repo.workdir().unwrap();
        std::fs::write(workdir.join("PKGBUILD"), format!("pkgver={pkgver}\n")).unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("PKGBUILD")).unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let sig = Signature::now("Test", "test@example.com").unwrap();
        let parents: Vec<_> = repo
            .head()
            .ok()
            .and_then(|h| h.target())
            .and_then(|oid| repo.find_commit(oid).ok())
            .into_iter()
            .collect();
        let parent_refs: Vec<_> = parents.iter().collect();
        repo.commit(Some("HEAD"), &sig, &sig, pkgver, &tree, &parent_refs)
            .unwrap();
    }

    /// Regression: a re-used checkout must follow upstream.
    ///
    /// `fetch` only moves remote-tracking refs, so resolving the *local* `HEAD`
    /// pinned the checkout to its first-cloned commit permanently. AUR sources
    /// resolve with `git_ref = "HEAD"`, so every AUR package's cached source was
    /// frozen at whatever it was when first fetched: AURCache would report an
    /// update available (from the live AUR RPC) while `package_update` read the
    /// stale pkgver off this checkout and refused with "Latest build is already
    /// up to date".
    #[test]
    fn reused_checkout_follows_upstream_head() {
        let upstream_dir = tempfile::tempdir().unwrap();
        let upstream = Repository::init(upstream_dir.path()).unwrap();
        commit(&upstream, "1.0");

        let cache = tempfile::tempdir().unwrap();
        let path = cache.path().join("pkg");
        let url = upstream_dir.path().to_string_lossy().to_string();

        let first = checkout_or_fetch_repo_ref(&url, "HEAD", &path).unwrap();
        assert!(
            std::fs::read_to_string(path.join("PKGBUILD"))
                .unwrap()
                .contains("1.0")
        );

        // The AUR maintainer pushes a new pkgver.
        commit(&upstream, "2.0");

        let second = checkout_or_fetch_repo_ref(&url, "HEAD", &path).unwrap();
        assert_ne!(first, second, "checkout did not advance to the new commit");
        assert!(
            std::fs::read_to_string(path.join("PKGBUILD"))
                .unwrap()
                .contains("2.0"),
            "working tree still holds the pre-update PKGBUILD"
        );
    }

    /// A named branch must track upstream across re-fetches too.
    /// Rewrite history on the upstream branch, as a force-push does: the new
    /// tip is not a descendant of the old one.
    fn force_push_unrelated_history(repo: &Repository, pkgver: &str) {
        let workdir = repo.workdir().unwrap();
        std::fs::write(workdir.join("PKGBUILD"), format!("pkgver={pkgver}\n")).unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("PKGBUILD")).unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let sig = Signature::now("Test", "test@example.com").unwrap();
        // No parents: an orphan commit, so it cannot be reached from the old tip.
        let rewritten = repo.commit(None, &sig, &sig, pkgver, &tree, &[]).unwrap();
        let branch = repo.head().unwrap().name().unwrap().to_string();
        repo.reference(&branch, rewritten, true, "force push")
            .unwrap();
    }

    /// A force-pushed upstream must be adopted, not treated as an error or
    /// quietly ignored. The cache is read-only and disposable, so the right
    /// behaviour is to hard-reset onto whatever the remote now says, even
    /// though it is not a fast-forward.
    #[test]
    fn reused_checkout_follows_a_force_pushed_branch() {
        let upstream_dir = tempfile::tempdir().unwrap();
        let upstream = Repository::init(upstream_dir.path()).unwrap();
        commit(&upstream, "1.0");

        let cache = tempfile::tempdir().unwrap();
        let path = cache.path().join("pkg");
        let url = upstream_dir.path().to_string_lossy().to_string();

        let first = checkout_or_fetch_repo_ref(&url, "HEAD", &path).unwrap();

        force_push_unrelated_history(&upstream, "9.9");
        let rewritten = upstream.head().unwrap().target().unwrap();
        assert_ne!(first, rewritten, "test did not actually rewrite history");

        let second = checkout_or_fetch_repo_ref(&url, "HEAD", &path).unwrap();
        assert_eq!(
            second, rewritten,
            "checkout did not reset onto the force-pushed commit"
        );
        assert!(
            std::fs::read_to_string(path.join("PKGBUILD"))
                .unwrap()
                .contains("9.9"),
            "working tree still holds the pre-force-push PKGBUILD"
        );
    }

    /// Some remotes do not advertise a `HEAD` symref, so the clone gets no
    /// `refs/remotes/origin/HEAD`. Resolving `HEAD` then fell back to a
    /// hardcoded `master`/`main` guess, and a repo using neither was left
    /// pinned to whatever commit it first cloned. Reading the branch name off
    /// the checkout works regardless of what the branch is called.
    #[test]
    fn reused_checkout_follows_upstream_without_an_origin_head_ref() {
        let upstream_dir = tempfile::tempdir().unwrap();
        let upstream = Repository::init(upstream_dir.path()).unwrap();
        commit(&upstream, "1.0");
        let original = upstream.head().unwrap().shorthand().unwrap().to_string();
        let head_commit = upstream.head().unwrap().peel_to_commit().unwrap();
        upstream.branch("trunk", &head_commit, true).unwrap();
        upstream.set_head("refs/heads/trunk").unwrap();
        upstream
            .find_branch(&original, git2::BranchType::Local)
            .unwrap()
            .delete()
            .unwrap();

        let cache = tempfile::tempdir().unwrap();
        let path = cache.path().join("pkg");
        let url = upstream_dir.path().to_string_lossy().to_string();

        let first = checkout_or_fetch_repo_ref(&url, "HEAD", &path).unwrap();

        // Drop the symref the clone recorded, leaving only `origin/trunk`.
        Repository::open(&path)
            .unwrap()
            .find_reference("refs/remotes/origin/HEAD")
            .unwrap()
            .delete()
            .unwrap();

        commit(&upstream, "2.0");

        let second = checkout_or_fetch_repo_ref(&url, "HEAD", &path).unwrap();
        assert_ne!(
            first, second,
            "checkout did not advance without an origin/HEAD to follow"
        );
        assert!(
            std::fs::read_to_string(path.join("PKGBUILD"))
                .unwrap()
                .contains("2.0")
        );
    }

    /// The default branch is whatever the remote says it is — not necessarily
    /// `master` or `main`. Resolving `HEAD` used to try a hardcoded list of
    /// those two names, so a repo using anything else fell through to whatever
    /// the stale local ref happened to be.
    #[test]
    fn reused_checkout_follows_an_unconventionally_named_default_branch() {
        let upstream_dir = tempfile::tempdir().unwrap();
        let upstream = Repository::init(upstream_dir.path()).unwrap();
        commit(&upstream, "1.0");
        // Rename the default branch to something neither hardcoded name would
        // match, and drop the original so it cannot be resolved instead.
        let original = upstream.head().unwrap().shorthand().unwrap().to_string();
        let head_commit = upstream.head().unwrap().peel_to_commit().unwrap();
        upstream.branch("trunk", &head_commit, true).unwrap();
        upstream.set_head("refs/heads/trunk").unwrap();
        upstream
            .find_branch(&original, git2::BranchType::Local)
            .unwrap()
            .delete()
            .unwrap();

        let cache = tempfile::tempdir().unwrap();
        let path = cache.path().join("pkg");
        let url = upstream_dir.path().to_string_lossy().to_string();

        let first = checkout_or_fetch_repo_ref(&url, "HEAD", &path).unwrap();

        commit(&upstream, "2.0");

        let second = checkout_or_fetch_repo_ref(&url, "HEAD", &path).unwrap();
        assert_ne!(
            first, second,
            "checkout did not follow a default branch named `trunk`"
        );
        assert!(
            std::fs::read_to_string(path.join("PKGBUILD"))
                .unwrap()
                .contains("2.0")
        );
    }

    #[test]
    fn reused_checkout_follows_named_branch() {
        let upstream_dir = tempfile::tempdir().unwrap();
        let upstream = Repository::init(upstream_dir.path()).unwrap();
        commit(&upstream, "1.0");
        let branch = upstream.head().unwrap().shorthand().unwrap().to_string();

        let cache = tempfile::tempdir().unwrap();
        let path = cache.path().join("pkg");
        let url = upstream_dir.path().to_string_lossy().to_string();

        let first = checkout_or_fetch_repo_ref(&url, &branch, &path).unwrap();
        commit(&upstream, "2.0");
        let second = checkout_or_fetch_repo_ref(&url, &branch, &path).unwrap();

        assert_ne!(first, second, "branch checkout did not advance");
        assert!(
            std::fs::read_to_string(path.join("PKGBUILD"))
                .unwrap()
                .contains("2.0")
        );
    }

    /// A pinned commit SHA must stay pinned even as upstream moves.
    #[test]
    fn pinned_commit_sha_does_not_move() {
        let upstream_dir = tempfile::tempdir().unwrap();
        let upstream = Repository::init(upstream_dir.path()).unwrap();
        commit(&upstream, "1.0");
        let pinned = upstream.head().unwrap().target().unwrap().to_string();

        let cache = tempfile::tempdir().unwrap();
        let path = cache.path().join("pkg");
        let url = upstream_dir.path().to_string_lossy().to_string();

        let first = checkout_or_fetch_repo_ref(&url, &pinned, &path).unwrap();
        commit(&upstream, "2.0");
        let second = checkout_or_fetch_repo_ref(&url, &pinned, &path).unwrap();

        assert_eq!(first, second, "pinned SHA must not follow upstream");
        assert!(
            std::fs::read_to_string(path.join("PKGBUILD"))
                .unwrap()
                .contains("1.0")
        );
    }

    /// An unknown ref must not fall through to HEAD: it used to resolve the
    /// default branch tip, so a mistyped branch/tag — or a `#tag=<sha>` pin —
    /// silently tracked upstream and rebuilt on every commit.
    #[test]
    fn unknown_refs_do_not_fall_back_to_head() {
        for git_ref in [
            "6e481d6bf0a69f8c9bd2866eb491e1e4e9b0717f",
            "no-such-branch",
            "v9.9.9",
        ] {
            assert!(
                !ls_remote_candidates(git_ref).contains(&"HEAD".to_string()),
                "{git_ref} must not resolve to HEAD"
            );
        }
        assert_eq!(ls_remote_candidates("HEAD"), vec!["HEAD".to_string()]);
        assert_eq!(
            ls_remote_candidates("main"),
            vec![
                "main".to_string(),
                "refs/heads/main".to_string(),
                "refs/tags/main".to_string(),
            ],
            "branches still prefer heads over tags, as the worker does"
        );
    }

    /// A tag is a pin too: it must resolve, and must not drift to the branch
    /// tip when upstream moves on.
    #[test]
    fn tag_ref_stays_at_the_tagged_commit() {
        let upstream_dir = tempfile::tempdir().unwrap();
        let upstream = Repository::init(upstream_dir.path()).unwrap();
        commit(&upstream, "1.0");
        let tagged = upstream.head().unwrap().target().unwrap();
        let obj = upstream.find_object(tagged, None).unwrap();
        upstream.tag_lightweight("v1.0", &obj, false).unwrap();

        let cache = tempfile::tempdir().unwrap();
        let path = cache.path().join("pkg");
        let url = upstream_dir.path().to_string_lossy().to_string();

        let first = checkout_or_fetch_repo_ref(&url, "v1.0", &path).unwrap();
        assert_eq!(first, tagged, "tag should resolve to the tagged commit");

        commit(&upstream, "2.0");
        let second = checkout_or_fetch_repo_ref(&url, "v1.0", &path).unwrap();

        assert_eq!(second, tagged, "tag must not drift to the branch tip");
        assert!(
            std::fs::read_to_string(path.join("PKGBUILD"))
                .unwrap()
                .contains("1.0")
        );
    }
}
