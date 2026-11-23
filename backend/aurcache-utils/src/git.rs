use anyhow::{anyhow, bail};
use aurcache_common::source::{SUPPORTED_GIT_URLS, SshRemote, Transport, transport};
use git2::{
    CertificateCheckStatus, Cred, CredentialType, Direction, FetchOptions, Oid, RemoteCallbacks,
    Repository,
};
use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;

use crate::ssh::{Identity, SshAccess};

/// `GIT_FEATURE_SSH`, from libgit2's `git2/common.h`.
const GIT_FEATURE_SSH: std::ffi::c_int = 1 << 2;

unsafe extern "C" {
    /// In libgit2 since 1.9, the oldest version that `libgit2-sys` 0.18 links.
    fn git_libgit2_feature_backend(feature: std::ffi::c_int) -> *const std::ffi::c_char;
}

/// Why the linked libgit2 cannot clone SSH sources, or `None` when it can.
///
/// Only its libssh2 backend calls the callbacks that hold the server's key
/// and host-key checks: its `exec` backend runs `ssh`, which would skip
/// both. The images link a vendored libgit2 built with libssh2; a native
/// install links the system's.
#[must_use]
pub fn ssh_unsupported() -> Option<String> {
    // SAFETY: reads how libgit2 was built, and returns a static string or
    // null; it needs no initialisation and takes no pointer.
    let name = unsafe { git_libgit2_feature_backend(GIT_FEATURE_SSH) };
    let backend = if name.is_null() {
        "none".into()
    } else {
        // SAFETY: non-null, it is a NUL-terminated static string.
        unsafe { std::ffi::CStr::from_ptr(name) }.to_string_lossy()
    };
    (backend != "libssh2").then(|| {
        format!(
            "the libgit2 that this server is linked with has SSH backend `{backend}`, which would \
             bypass the server's key and host-key checks; SSH sources need one built with \
             libssh2, as the container images' is"
        )
    })
}

/// What a git operation may reach its remote with.
#[derive(Clone, Copy, Debug)]
pub enum Access<'a> {
    /// The server's SSH key and host-key checks: for a package's own source.
    Server(&'a SshAccess),
    /// No credential at all, for remotes that a PKGBUILD names. An SSH remote
    /// is refused before connecting.
    Anonymous,
}

/// How one URL is reached.
#[derive(Debug)]
enum Route<'a> {
    /// Not over SSH.
    Default,
    Ssh {
        remote: SshRemote,
        identity: &'a Identity,
        access: &'a SshAccess,
    },
}

/// Decide how `url` is reached with `access`, or why it cannot be.
fn route<'a>(url: &str, access: Access<'a>) -> anyhow::Result<Route<'a>> {
    // The URL is never repeated in a refusal: it can carry a password.
    let remote = match transport(url) {
        None => bail!("unsupported git URL: {SUPPORTED_GIT_URLS}"),
        Some(Transport::Other) => return Ok(Route::Default),
        Some(Transport::Ssh(remote)) => remote,
    };
    let host = remote.known_hosts_name();
    let Access::Server(access) = access else {
        bail!(
            "{host} is reached over SSH, and the server offers its SSH key only for a \
             package's own source; workers fetch the sources that a PKGBUILD downloads"
        );
    };
    let identity = access
        .identity()
        .map_err(|reason| anyhow!("cloning from {host} needs the server's SSH key: {reason}"))?;
    Ok(Route::Ssh {
        remote,
        identity,
        access,
    })
}

/// Why a callback refused a connection.
///
/// libgit2 replaces a callback's error with a generic one ("invalid or
/// unknown remote ssh hostkey"), so the reason is kept here and reported
/// instead.
#[derive(Clone, Default)]
struct Refusal(Rc<RefCell<Option<String>>>);

impl Refusal {
    fn refuse(&self, why: String) -> git2::Error {
        let error = git2::Error::from_str(&why);
        *self.0.borrow_mut() = Some(why);
        error
    }

    /// `error`, or the reason that a callback gave for it.
    fn explain(&self, error: git2::Error) -> anyhow::Error {
        match self.0.borrow_mut().take() {
            Some(why) => anyhow!(why),
            None => error.into(),
        }
    }
}

/// The key and the host-key check for an SSH route. Every route checks
/// certificates, so that nothing reaches SSH without the host-key check.
fn callbacks<'a>(route: &Route<'a>, refusal: &Refusal) -> RemoteCallbacks<'a> {
    let mut callbacks = RemoteCallbacks::new();
    let Route::Ssh {
        remote,
        identity,
        access,
    } = route
    else {
        let refusal = refusal.clone();
        callbacks.certificate_check(move |cert, host| match cert.as_hostkey() {
            Some(_) => Err(refusal.refuse(format!(
                "reached {host} over SSH for a URL that is not an SSH one"
            ))),
            None => Ok(CertificateCheckStatus::CertificatePassthrough),
        });
        return callbacks;
    };
    let (identity, access) = (*identity, *access);
    let name = remote.known_hosts_name();

    let user = remote.user.clone().unwrap_or_else(|| "git".to_string());
    let rejected = format!(
        "{name} refused the server's SSH key ({}): add it to the repository as a deploy \
         key; the settings page shows it, and `aurcli ssh key` prints it",
        identity.fingerprint()
    );
    // libgit2 asks again after a refusal for as long as the callback
    // answers; there is one key, so a second request is the answer.
    let mut offered = false;
    let credentials_refusal = refusal.clone();
    callbacks.credentials(move |_url, from_url, allowed| {
        let user = from_url.unwrap_or(&user);
        if allowed.contains(CredentialType::USERNAME) {
            return Cred::username(user);
        }
        if !allowed.contains(CredentialType::SSH_KEY) || offered {
            return Err(credentials_refusal.refuse(rejected.clone()));
        }
        offered = true;
        Cred::ssh_key(user, None, identity.private_key(), None)
    });

    let remote = remote.clone();
    let refusal = refusal.clone();
    callbacks.certificate_check(move |cert, host| {
        let Some(hostkey) = cert.as_hostkey() else {
            // A TLS certificate: libgit2 checks those itself.
            return Ok(CertificateCheckStatus::CertificatePassthrough);
        };
        // The key is judged under the name read from the URL, so it must be
        // the host that libgit2 reached.
        if !host.eq_ignore_ascii_case(&remote.host) {
            return Err(refusal.refuse(format!("connected to {host} while checking {name}")));
        }
        let verdict = match hostkey.hostkey().map(ssh_key::PublicKey::from_bytes) {
            Some(Ok(key)) => access.hosts().verify(&remote, &key),
            Some(Err(e)) => Err(anyhow!("{name} sent a host key that does not parse: {e}")),
            None => Err(anyhow!("{name} sent no host key")),
        };
        verdict
            .map(|()| CertificateCheckStatus::CertificateOk)
            .map_err(|e| refusal.refuse(format!("{e:#}")))
    });
    callbacks
}

fn fetch_options<'a>(route: &Route<'a>, refusal: &Refusal) -> FetchOptions<'a> {
    let mut options = FetchOptions::new();
    options.remote_callbacks(callbacks(route, refusal));
    options
}

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
    access: Access<'_>,
) -> anyhow::Result<Oid> {
    let route = route(git_repo, access)?;
    let refusal = Refusal::default();
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
            remote
                .fetch(
                    &[] as &[&str],
                    Some(&mut fetch_options(&route, &refusal)),
                    None,
                )
                .map_err(|e| refusal.explain(e))?;
            let _ = remote.fetch(&[git_ref], Some(&mut fetch_options(&route, &refusal)), None);
        }
        repo
    } else {
        std::fs::create_dir_all(path)?;
        git2::build::RepoBuilder::new()
            .fetch_options(fetch_options(&route, &refusal))
            .clone(git_repo, path)
            .map_err(|e| refusal.explain(e))?
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
/// it was asked for: falling through to the remote's `HEAD` for an unknown ref
/// would make a mistyped branch/tag — or a `#tag=<sha>` pin — silently track
/// the default branch tip and rebuild on every upstream commit.
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
pub fn ls_remote(git_repo: &str, git_ref: &str, access: Access<'_>) -> anyhow::Result<String> {
    let route = route(git_repo, access)?;
    // A throwaway repository is required to create a remote in git2, even
    // for a purely in-memory listing; it performs no disk I/O for the actual
    // remote sources.
    let dir = tempfile::tempdir()?;
    let repo = Repository::init_bare(dir.path())?;
    let mut remote = repo.remote_anonymous(git_repo)?;
    let refusal = Refusal::default();
    remote
        .connect_auth(Direction::Fetch, Some(callbacks(&route, &refusal)), None)
        .map_err(|e| refusal.explain(e))?;

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
    use crate::ssh::{HostKeySource, KeySource};
    use aurcache_activitylog::activity_utils::ActivityLog;
    use git2::Signature;

    /// Only an SSH remote, and only with the server's access, is offered
    /// the key; a URL in no supported form is refused before connecting.
    #[test]
    fn urls_are_routed_by_their_form() {
        let dir = tempfile::tempdir().unwrap();
        let access = SshAccess::new(
            KeySource::Generated(dir.path().join("id_ed25519")),
            HostKeySource::TrustOnFirstUse(dir.path().join("known_hosts")),
            ActivityLog::discarding(),
        );
        for access in [Access::Server(&access), Access::Anonymous] {
            for url in ["https://github.com/user/repo.git", "/srv/git/repo.git"] {
                assert!(matches!(route(url, access), Ok(Route::Default)), "{url}");
            }
            let url = "ssh://deploy:hunter2@host/repo";
            let error = route(url, access).unwrap_err().to_string();
            assert!(error.contains("unsupported git URL"), "{error}");
            assert!(!error.contains("hunter2"), "the URL is repeated: {error}");
        }
        assert!(matches!(
            route("git@github.com:u/r.git", Access::Server(&access)),
            Ok(Route::Ssh { .. })
        ));
        let error = route("git@github.com:u/r.git", Access::Anonymous)
            .unwrap_err()
            .to_string();
        assert!(error.contains("workers fetch"), "{error}");

        let none = SshAccess::unavailable("AURCACHE_GIT_SSH_KEY points at /nope");
        let error = route("git@github.com:u/r.git", Access::Server(&none))
            .unwrap_err()
            .to_string();
        assert!(error.contains("points at /nope"), "{error}");
    }

    /// The system libgit2 or the vendored one: either must be libssh2, or
    /// every SSH test would pass against a server that cannot use SSH.
    #[test]
    fn the_linked_libgit2_reaches_ssh_through_libssh2() {
        assert_eq!(ssh_unsupported(), None);
    }

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

        let first = checkout_or_fetch_repo_ref(&url, "HEAD", &path, Access::Anonymous).unwrap();
        assert!(
            std::fs::read_to_string(path.join("PKGBUILD"))
                .unwrap()
                .contains("1.0")
        );

        // The AUR maintainer pushes a new pkgver.
        commit(&upstream, "2.0");

        let second = checkout_or_fetch_repo_ref(&url, "HEAD", &path, Access::Anonymous).unwrap();
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

        let first = checkout_or_fetch_repo_ref(&url, "HEAD", &path, Access::Anonymous).unwrap();

        force_push_unrelated_history(&upstream, "9.9");
        let rewritten = upstream.head().unwrap().target().unwrap();
        assert_ne!(first, rewritten, "test did not actually rewrite history");

        let second = checkout_or_fetch_repo_ref(&url, "HEAD", &path, Access::Anonymous).unwrap();
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

        let first = checkout_or_fetch_repo_ref(&url, "HEAD", &path, Access::Anonymous).unwrap();

        // Drop the symref the clone recorded, leaving only `origin/trunk`.
        Repository::open(&path)
            .unwrap()
            .find_reference("refs/remotes/origin/HEAD")
            .unwrap()
            .delete()
            .unwrap();

        commit(&upstream, "2.0");

        let second = checkout_or_fetch_repo_ref(&url, "HEAD", &path, Access::Anonymous).unwrap();
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
    /// `master` or `main`. Resolving `HEAD` through a hardcoded list of those
    /// two names would leave a repo using anything else on whatever the stale
    /// local ref happened to be.
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

        let first = checkout_or_fetch_repo_ref(&url, "HEAD", &path, Access::Anonymous).unwrap();

        commit(&upstream, "2.0");

        let second = checkout_or_fetch_repo_ref(&url, "HEAD", &path, Access::Anonymous).unwrap();
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

        let first = checkout_or_fetch_repo_ref(&url, &branch, &path, Access::Anonymous).unwrap();
        commit(&upstream, "2.0");
        let second = checkout_or_fetch_repo_ref(&url, &branch, &path, Access::Anonymous).unwrap();

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

        let first = checkout_or_fetch_repo_ref(&url, &pinned, &path, Access::Anonymous).unwrap();
        commit(&upstream, "2.0");
        let second = checkout_or_fetch_repo_ref(&url, &pinned, &path, Access::Anonymous).unwrap();

        assert_eq!(first, second, "pinned SHA must not follow upstream");
        assert!(
            std::fs::read_to_string(path.join("PKGBUILD"))
                .unwrap()
                .contains("1.0")
        );
    }

    /// An unknown ref must not fall through to HEAD: resolving the default
    /// branch tip instead, a mistyped branch/tag — or a `#tag=<sha>` pin —
    /// would silently track upstream and rebuild on every commit.
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

        let first = checkout_or_fetch_repo_ref(&url, "v1.0", &path, Access::Anonymous).unwrap();
        assert_eq!(first, tagged, "tag should resolve to the tagged commit");

        commit(&upstream, "2.0");
        let second = checkout_or_fetch_repo_ref(&url, "v1.0", &path, Access::Anonymous).unwrap();

        assert_eq!(second, tagged, "tag must not drift to the branch tip");
        assert!(
            std::fs::read_to_string(path.join("PKGBUILD"))
                .unwrap()
                .contains("1.0")
        );
    }
}
