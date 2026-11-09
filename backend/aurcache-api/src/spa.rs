//! Deciding when an unmatched request should be answered with the app shell.
//!
//! The frontend is a single-page app: `/builds` and `/package/hello` are routes
//! it renders itself, not files. Nothing on disk matches them, so a reload or a
//! pasted link only works if the server answers unknown paths with
//! `index.html` and lets the frontend read the URL.
//!
//! Kept out of `embed`, and out of the `static` feature it is used under, so
//! the rule can be tested without building the embedded frontend.

use std::path::Path;

/// Whether a request that matched no embedded asset should get the app shell.
///
/// The one exclusion is `/api`. Those routes are mounted separately and win on
/// specificity, so only a path *no* API route matched arrives here — a typo, or
/// a client on a newer version. Answering that with HTML would turn a clear 404
/// into a parse error somewhere further away, so it stays a 404.
///
/// Deliberately not filtered by file extension, which is the usual shortcut for
/// this. Package names are part of these URLs and routinely contain dots —
/// `python-3.11`, `2048.c` — so "has an extension" would classify real UI
/// routes as missing files and 404 them. The cost of not filtering is that a
/// genuinely missing asset returns the shell instead of a 404, which only
/// happens when the frontend build is broken.
pub fn serves_app_shell(path: &Path) -> bool {
    !path.starts_with("api")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn shell_for(path: &str) -> bool {
        serves_app_shell(&PathBuf::from(path))
    }

    /// The reason the fallback exists: these are frontend routes, and a reload
    /// or a pasted link has to reach the app rather than a 404.
    #[test]
    fn frontend_routes_get_the_app_shell() {
        assert!(shell_for("builds"));
        assert!(shell_for("build/12"));
        assert!(shell_for("packages"));
        assert!(shell_for("package/hello"));
        assert!(shell_for("package/hello/source/PKGBUILD"));
        assert!(shell_for("settings"));
    }

    /// An unmatched API path is a real 404. Serving HTML there would surface as
    /// a JSON parse error in a client instead of the status it asked about.
    #[test]
    fn unmatched_api_paths_stay_a_404() {
        assert!(!shell_for("api"));
        assert!(!shell_for("api/packages/nope"));
        assert!(!shell_for("api/v2/whatever"));
    }

    /// Package names contain dots, so an extension-based rule would 404 these.
    /// `2048.c` is a real package name; `python-3.11` parses as extension
    /// `.11`. Both are ordinary UI routes.
    #[test]
    fn package_names_that_look_like_filenames_still_get_the_shell() {
        assert!(shell_for("package/2048.c"));
        assert!(shell_for("package/python-3.11"));
        assert!(shell_for("package/lib32-glibc"));
        assert!(shell_for("package/gtk3+"));
    }

    /// Only the `api` segment is special, not any path that merely starts with
    /// those letters.
    #[test]
    fn only_the_api_segment_is_excluded() {
        assert!(shell_for("apiary"));
        assert!(shell_for("package/api"));
    }
}
