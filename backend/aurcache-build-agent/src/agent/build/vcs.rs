//! What each of the build's `git+` sources was at, read from the mirrors
//! makepkg fetched into `SRCDEST`.
//!
//! Asked as the build user: git on a repository the build wrote reads
//! configuration the build chose. And asked only once the build's processes
//! are dead, so nothing moves a ref under the question.

use super::run::{self, Keep};
use super::user::BuildUser;
use aurcache_build_agent::layout::Scratch;
use aurcache_common::source::ref_candidates;
use aurcache_common::worker::JobVcsSource;
use std::collections::BTreeMap;
use std::path::{Component, Path};
use std::time::Duration;

/// How long one question to git may take.
const PATIENCE: Duration = Duration::from_secs(30);

/// Resolve each source against its mirror, keyed by its source string.
///
/// One that cannot be resolved is left out: the server then keeps the commit
/// it recorded when it queued the build.
pub fn commits(
    sources: &[JobVcsSource],
    scratch: &Scratch,
    user: &BuildUser,
) -> BTreeMap<String, String> {
    let mut resolved = BTreeMap::new();
    for source in sources {
        if !is_one_name(&source.dir) {
            continue;
        }
        let mirror = scratch.srcdest().join(&source.dir);
        if let Some(commit) = rev_parse(&mirror, &source.git_ref, user) {
            resolved.insert(source.source_url.clone(), commit);
        }
    }
    resolved
}

/// Ask git what a ref points at, without peeling it, trying its candidates
/// in the order the server does.
fn rev_parse(mirror: &Path, git_ref: &str, user: &BuildUser) -> Option<String> {
    ref_candidates(git_ref).into_iter().find_map(|candidate| {
        let mut git = user.command("git");
        git.arg("-C")
            .arg(mirror)
            .args(["rev-parse", "--verify", "--quiet", "--end-of-options"])
            .arg(candidate);
        let asked = run::captured(&mut git, Keep::Stdout, Some(PATIENCE)).ok()?;
        let answer = asked.text.trim();
        (asked.succeeded() && is_object_name(answer)).then(|| answer.to_string())
    })
}

/// Whether a source's directory is a single name under `SRCDEST`.
fn is_one_name(dir: &str) -> bool {
    let mut parts = Path::new(dir).components();
    matches!(parts.next(), Some(Component::Normal(_))) && parts.next().is_none()
}

/// Whether git's answer is an object name: SHA-1 or SHA-256, in hexadecimal.
fn is_object_name(answer: &str) -> bool {
    matches!(answer.len(), 40 | 64) && answer.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::{is_object_name, is_one_name};

    #[test]
    fn a_mirror_is_one_name_under_srcdest() {
        assert!(is_one_name("repo"));
        assert!(is_one_name("linux-git"));
        assert!(!is_one_name(""));
        assert!(!is_one_name(".."));
        assert!(!is_one_name("."));
        assert!(!is_one_name("a/b"));
        assert!(!is_one_name("../srcdest-other/repo"));
        assert!(!is_one_name("/etc"));
    }

    #[test]
    fn only_an_object_name_is_reported() {
        assert!(is_object_name("9fceb02d0ae598e95dc970b74767f19372d61af8"));
        assert!(is_object_name(&"a".repeat(64)));
        assert!(!is_object_name(""));
        assert!(!is_object_name("9fceb02"));
        assert!(!is_object_name(
            "9fceb02d0ae598e95dc970b74767f19372d61af8\nand more"
        ));
        assert!(!is_object_name(&"g".repeat(40)));
    }
}
