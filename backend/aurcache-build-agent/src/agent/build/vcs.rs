//! What each of the build's `git+` sources was at, read from the mirrors
//! that makepkg fetched into `SRCDEST`.
//!
//! Asked as the build user: git on a repository that the build wrote reads
//! configuration that the build chose. And asked only once the build's processes
//! are dead, so nothing moves a ref under the question.

use super::run;
use super::user::BuildUser;
use aurcache_build_agent::layout::Scratch;
use aurcache_common::source::ref_candidates;
use aurcache_common::worker::JobVcsSource;
use std::collections::BTreeMap;
use std::path::Path;

/// The most git's answer may be: an object name and a newline.
const ANSWER_MAX: u64 = 128;

/// Resolve each source against its mirror, keyed by its source string.
///
/// One that cannot be resolved is left out: the server then keeps the commit
/// that it recorded when it queued the build.
pub fn commits(
    sources: &[JobVcsSource],
    scratch: &Scratch,
    user: &BuildUser,
) -> BTreeMap<String, String> {
    let mut resolved = BTreeMap::new();
    for source in sources {
        if !super::is_one_name(&source.dir) {
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
/// in the server's order.
fn rev_parse(mirror: &Path, git_ref: &str, user: &BuildUser) -> Option<String> {
    ref_candidates(git_ref).into_iter().find_map(|candidate| {
        let mut git = user.command("git");
        git.arg("-C")
            .arg(mirror)
            .args(["rev-parse", "--verify", "--quiet", "--end-of-options"])
            .arg(candidate);
        let asked = run::printed(&mut git, ANSWER_MAX).ok()?;
        let answer = String::from_utf8_lossy(&asked.stdout);
        let answer = answer.trim();
        (asked.status.success() && is_object_name(answer)).then(|| answer.to_string())
    })
}

/// Whether `text` is a git object name: SHA-1 or SHA-256, in hexadecimal.
/// Anything else was not git's answer.
fn is_object_name(text: &str) -> bool {
    matches!(text.len(), 40 | 64) && text.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::is_object_name;

    #[test]
    fn only_an_object_name_is_one() {
        assert!(is_object_name("9fceb02d0ae598e95dc970b74767f19372d61af8"));
        assert!(is_object_name(&"a".repeat(64)));
        for not in ["", "9fceb02", "9fceb02d0ae598e95dc970b74767f19372d61af8\n"] {
            assert!(!is_object_name(not), "{not:?}");
        }
        assert!(!is_object_name(&"g".repeat(40)));
    }
}
