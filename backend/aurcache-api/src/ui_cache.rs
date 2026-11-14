//! How browsers may cache the embedded web UI.
//!
//! Without any of this, every full page load fetched the whole bundle again:
//! the wasm, its glue, and the vendored stylesheet and Tailwind script, about a
//! megabyte even compressed. With it, a reload revalidates each file and gets
//! `304 Not Modified` back, and the vendored files are not asked for at all.
//!
//! Kept out of `embed`, and out of the `static` feature it is used under, so
//! the rules can be tested without building the embedded frontend.

use std::path::Path;

/// How long a response may be reused without asking the server again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CachePolicy {
    /// Never changes at this path: `vendor/` names its files by version, so a
    /// new version is a new URL and the old one can be kept indefinitely.
    Immutable,
    /// May change with any upgrade under the same name -- the page, the wasm
    /// and its glue -- so reuse it only after the server confirms it.
    Revalidate,
}

impl CachePolicy {
    /// The policy for an embedded file, by its path in the bundle.
    pub fn of(path: &Path) -> Self {
        if path.starts_with("vendor") {
            Self::Immutable
        } else {
            Self::Revalidate
        }
    }

    /// The `Cache-Control` value.
    pub fn header(self) -> &'static str {
        match self {
            Self::Immutable => "max-age=31536000, immutable",
            // `no-cache` stores the response but revalidates it before every
            // use, which is what makes the ETag pay off. `no-store` would
            // refetch the whole body each time.
            Self::Revalidate => "no-cache",
        }
    }
}

/// The entity tag for a file, from the SHA-256 `rust-embed` stores with it.
///
/// Weak, because the compression fairing in `init_api` may encode the body
/// after this handler has answered, and a strong tag promises the bytes on the
/// wire. A weak one promises the content, which is all a revalidation needs.
pub fn etag(sha256: &[u8; 32]) -> String {
    format!("W/\"{}\"", hex::encode(&sha256[..16]))
}

/// Whether `If-None-Match` names `etag`, so the client's copy is current.
///
/// Weak comparison, as RFC 9110 requires for this header: `W/` is ignored on
/// both sides. `*` matches any current representation.
pub fn not_modified(if_none_match: Option<&str>, etag: &str) -> bool {
    let header = match if_none_match {
        Some(header) => header,
        None => return false,
    };
    let ours = opaque(etag);
    header
        .split(',')
        .map(str::trim)
        .any(|tag| tag == "*" || opaque(tag) == ours)
}

/// An entity tag without its weakness marker.
fn opaque(tag: &str) -> &str {
    tag.strip_prefix("W/").unwrap_or(tag)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: [u8; 32] = [0xab; 32];

    #[test]
    fn vendored_files_are_kept_and_everything_else_revalidated() {
        assert_eq!(
            CachePolicy::of(Path::new("vendor/daisyui-4.12.14.css")),
            CachePolicy::Immutable
        );
        for path in ["index.html", "aurcache-frontend_bg.wasm", "icon.svg"] {
            assert_eq!(CachePolicy::of(Path::new(path)), CachePolicy::Revalidate);
        }
    }

    /// A package page is the app shell, which must not be kept: an upgrade
    /// changes it under the same URL.
    #[test]
    fn a_route_named_like_vendor_is_not_the_vendor_directory() {
        assert_eq!(
            CachePolicy::of(Path::new("package/vendor-tool")),
            CachePolicy::Revalidate
        );
    }

    #[test]
    fn the_tag_is_weak_and_quoted() {
        assert_eq!(etag(&HASH), format!("W/\"{}\"", "ab".repeat(16)));
    }

    #[test]
    fn a_matching_tag_is_not_modified_however_it_is_marked() {
        let tag = etag(&HASH);
        let strong = tag.trim_start_matches("W/");
        assert!(not_modified(Some(&tag), &tag));
        assert!(not_modified(Some(strong), &tag));
        assert!(not_modified(Some(&format!("\"other\", {tag}")), &tag));
        assert!(not_modified(Some("*"), &tag));
    }

    #[test]
    fn anything_else_is_sent_in_full() {
        let tag = etag(&HASH);
        assert!(!not_modified(None, &tag));
        assert!(!not_modified(Some("W/\"other\""), &tag));
        assert!(!not_modified(Some(""), &tag));
    }
}
