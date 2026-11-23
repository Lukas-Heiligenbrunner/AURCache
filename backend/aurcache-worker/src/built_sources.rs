//! What a build's `git+` sources were actually at, read from this worker's own
//! `SRCDEST` mirrors.
//!
//! The server records a commit when it *queues* a build, resolved from the
//! remote; by the time the build runs, upstream may have moved. This is the
//! same question answered from the copy the build was made from, so the record
//! stops being a good guess and becomes what happened.
//!
//! Read from the mirror rather than from the working copy because the working
//! copy dies with the chroot: `makechrootpkg -T` deletes the copy, and
//! `$srcdir` is inside it unless the package happens to keep a persistent build
//! tree. The mirror is this worker's, always present, and holds exactly what
//! makepkg fetched and then cloned the working copy from.
//!
//! Called while the job still holds its `SRCDEST` guard
//! (`crate::srcdest_lock`), so no sibling build can fetch into the mirror
//! between the build ending and this read.

use aurcache_common::source::ref_candidates;
use aurcache_common::worker::JobVcsSource;
use std::collections::BTreeMap;
use std::path::Path;
use tokio::process::Command;

/// Resolve each source against the mirror it was fetched into.
///
/// Best-effort per source: one that cannot be resolved is left out, and the
/// server keeps the commit it recorded when it queued the build -- the older of
/// the two, so the error is a redundant rebuild rather than a missed one.
pub async fn resolve(sources: &[JobVcsSource], srcdest: Option<&Path>) -> BTreeMap<String, String> {
    let mut resolved = BTreeMap::new();
    let Some(srcdest) = srcdest else {
        return resolved;
    };
    for source in sources {
        let dir = srcdest.join(&source.dir);
        if !dir.is_dir() {
            continue;
        }
        if let Some(commit) = rev_parse(&dir, &source.git_ref).await {
            resolved.insert(source.source_url.clone(), commit);
        }
    }
    resolved
}

/// Ask git what a ref points at, without peeling it, trying its candidates in
/// the order the server does ([`ref_candidates`]).
///
/// Un-peeled deliberately: `ls_remote` reports the object a ref advertises, so
/// for an annotated tag it reports the tag object. Peeling here would report
/// the commit instead, and the two would never compare equal.
async fn rev_parse(dir: &Path, git_ref: &str) -> Option<String> {
    for candidate in ref_candidates(git_ref) {
        let output = Command::new("git")
            // The mirror belongs to the build user, not to the worker, so git
            // refuses it as "dubious ownership" without this.
            .arg("-c")
            .arg(format!("safe.directory={}", dir.display()))
            .arg("-c")
            .arg("safe.bareRepository=all")
            .arg("-C")
            .arg(dir)
            .args(["rev-parse", "--verify", "--quiet", &candidate])
            .output()
            .await
            .ok()?;
        if output.status.success() {
            let commit = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !commit.is_empty() {
                return Some(commit);
            }
        }
    }
    tracing::debug!("no ref for {git_ref} in {}", dir.display());
    None
}

#[cfg(test)]
mod tests {
    use super::resolve;
    use aurcache_common::worker::JobVcsSource;

    /// A worker with no source cache, or a source that was never fetched,
    /// reports nothing rather than guessing.
    #[tokio::test]
    async fn an_absent_mirror_resolves_to_nothing() {
        let sources = [JobVcsSource {
            source_url: "git+https://example.test/repo.git".into(),
            dir: "repo".into(),
            git_ref: "HEAD".into(),
        }];
        assert!(resolve(&sources, None).await.is_empty());

        let tmp = tempfile::tempdir().unwrap();
        assert!(resolve(&sources, Some(tmp.path())).await.is_empty());
    }

    /// The real thing, against a mirror made here: the commit reported is the
    /// one the ref points at.
    #[tokio::test]
    async fn a_mirror_reports_what_its_ref_points_at() {
        let tmp = tempfile::tempdir().unwrap();
        let origin = tmp.path().join("origin");
        let sh = |args: &[&str], cwd: &std::path::Path| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(cwd)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .output()
                .expect("git")
        };
        std::fs::create_dir(&origin).unwrap();
        sh(&["init", "-q", "-b", "main"], &origin);
        std::fs::write(origin.join("f"), "x").unwrap();
        sh(&["add", "f"], &origin);
        sh(&["commit", "-qm", "one"], &origin);
        let head = String::from_utf8(sh(&["rev-parse", "HEAD"], &origin).stdout).unwrap();
        let head = head.trim().to_string();

        let srcdest = tmp.path().join("srcdest");
        std::fs::create_dir(&srcdest).unwrap();
        sh(
            &["clone", "--mirror", "-q", origin.to_str().unwrap(), "repo"],
            &srcdest,
        );

        let sources = [JobVcsSource {
            source_url: "git+file:///origin".into(),
            dir: "repo".into(),
            git_ref: "main".into(),
        }];
        let resolved = resolve(&sources, Some(&srcdest)).await;
        assert_eq!(resolved.get("git+file:///origin"), Some(&head));
    }
}
