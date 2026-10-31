//! Append-only file storage for a build's log output.
//!
//! Logs used to live in `builds.output` and grow by `SET output = output || $1`.
//! That reads as an O(1) append and is anything but: under MVCC the whole value
//! is detoasted, decompressed, concatenated, recompressed and written as a new
//! row version, leaving the old one for VACUUM. Measured against a 30 MB value,
//! twenty appends took 3.7 s where twenty appends to an empty value took 0.3 ms
//! -- four orders of magnitude, paid on every flush, growing with the log. A
//! large build wrote gigabytes to store megabytes.
//!
//! A file appends in O(new bytes) and is read back by seeking to an offset, so
//! both directions become proportional to what actually changed. Byte offsets
//! are also unambiguous in a way the old line offsets were not: `substr` on
//! `text` counts codepoints and has to decode UTF-8 from the start to find the
//! Nth one, which measured 6x slower than slicing bytes even before the
//! transfer cost. And it removes the one dialect branch that lived in a query
//! path rather than a migration.

use std::io::SeekFrom;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tracing::warn;

/// Maximum bytes a single [`read_build_output`] call may return, whatever
/// limit is asked for: the bound of an absent limit, and the clamp of any
/// limit. Structural — no code path fetches a whole log at once, so the size
/// of one request never grows with the log.
pub const MAX_OUTPUT_BYTES: u64 = 32 << 20;

/// Clamp a caller's `limit` to [`MAX_OUTPUT_BYTES`].
fn resolved_limit(limit: Option<u64>) -> u64 {
    limit.map_or(MAX_OUTPUT_BYTES, |l| l.min(MAX_OUTPUT_BYTES))
}

pub use aurcache_common::fs::{build_log_dir, build_log_path, build_log_root};

/// Append `text` to a build's log, creating the file on first write.
pub async fn append_build_output(pkgbase: &str, number: i32, text: &str) -> anyhow::Result<()> {
    let path = build_log_path(pkgbase, number);
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .await?;
    file.write_all(text.as_bytes()).await?;
    Ok(())
}

/// Read up to `limit` bytes of a build's log from `offset` onwards. The result
/// is raw file bytes, not decoded text: alignment to a UTF-8 boundary is the
/// caller's job, because only the caller knows how much of a trailing character
/// it can afford to re-read next time.
///
/// `limit` defaults to, and is clamped to, [`MAX_OUTPUT_BYTES`], so a request
/// never pays in proportion to the log's size.
///
/// `Ok(None)` means there is no log file: a build that never produced output,
/// or one whose file has been removed. That is a normal answer rather than an
/// error, because the alternative is a 500 on a page whose entire job is to
/// display whatever there is. An empty `Some` is likewise normal: `offset`
/// past the end of the log and at or before `limit` bytes from it.
pub async fn read_build_output(
    pkgbase: &str,
    number: i32,
    offset: u64,
    limit: Option<u64>,
) -> anyhow::Result<Option<Vec<u8>>> {
    let path = build_log_path(pkgbase, number);
    let mut file = match tokio::fs::File::open(&path).await {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };

    if offset > 0 {
        file.seek(SeekFrom::Start(offset)).await?;
    }
    let mut buf = Vec::new();
    file.take(resolved_limit(limit))
        .read_to_end(&mut buf)
        .await?;

    Ok(Some(buf))
}

/// Size of a build's log in bytes, or `None` if it has none.
pub async fn build_log_size(pkgbase: &str, number: i32) -> Option<u64> {
    tokio::fs::metadata(build_log_path(pkgbase, number))
        .await
        .ok()
        .map(|m| m.len())
}

/// Delete every log belonging to one package.
///
/// One directory removal, which is the practical payoff of naming logs after
/// the build identity rather than the id: the old layout needed the ids of
/// every build read out before its rows were deleted.
///
/// Best-effort and never fatal: the rows are going away regardless, and a log
/// left behind is wasted bytes rather than a correctness problem. Deleting the
/// rows is what the caller must not skip.
pub async fn remove_package_logs(pkgbase: &str) {
    let dir = build_log_dir(pkgbase);
    match tokio::fs::remove_dir_all(&dir).await {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => warn!("could not remove build logs {}: {e}", dir.display()),
    }
}

/// Delete one build's log.
///
/// Not optional when a build row goes: numbers are `max + 1` within the
/// package, so the next build can take a deleted build's number and would
/// otherwise append to its log. Best-effort like [`remove_package_logs`].
pub async fn remove_build_log(pkgbase: &str, number: i32) {
    let path = build_log_path(pkgbase, number);
    match tokio::fs::remove_file(&path).await {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => warn!("could not remove build log {}: {e}", path.display()),
    }
}

/// Delete every build log there is, for a wipe that removes every package.
pub async fn remove_all_logs() {
    let root = build_log_root();
    match tokio::fs::remove_dir_all(&root).await {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => warn!("could not remove build logs {}: {e}", root.display()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The log root is read from the environment on every call, so tests that
    /// set it must not run concurrently.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct TempRoot {
        dir: tempfile::TempDir,
        _guard: std::sync::MutexGuard<'static, ()>,
    }

    fn temp_root() -> TempRoot {
        let guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("AURCACHE_BUILD_LOG_PATH", dir.path()) };
        TempRoot { dir, _guard: guard }
    }

    #[tokio::test]
    async fn append_creates_and_extends_the_file() {
        let root = temp_root();
        append_build_output("hello", 1, "hello\n").await.unwrap();
        append_build_output("hello", 1, "world\n").await.unwrap();

        assert_eq!(
            read_build_output("hello", 1, 0, None)
                .await
                .unwrap()
                .as_deref(),
            Some(b"hello\nworld\n".as_slice())
        );
        // Named after the build's public identity, not its row id.
        assert!(root.dir.path().join("hello/1.log").exists());
    }

    /// Builds of one package share a directory, and each number is its own file.
    #[tokio::test]
    async fn builds_of_a_package_are_separate_files() {
        let _root = temp_root();
        append_build_output("hello", 1, "first build\n")
            .await
            .unwrap();
        append_build_output("hello", 2, "second build\n")
            .await
            .unwrap();

        assert_eq!(
            read_build_output("hello", 1, 0, None)
                .await
                .unwrap()
                .as_deref(),
            Some(b"first build\n".as_slice())
        );
        assert_eq!(
            read_build_output("hello", 2, 0, None)
                .await
                .unwrap()
                .as_deref(),
            Some(b"second build\n".as_slice())
        );
    }

    #[tokio::test]
    async fn reading_from_an_offset_returns_only_the_tail() {
        let _root = temp_root();
        append_build_output("p", 1, "first\nsecond\n")
            .await
            .unwrap();

        let offset = u64::try_from("first\n".len()).unwrap();
        assert_eq!(
            read_build_output("p", 1, offset, None)
                .await
                .unwrap()
                .as_deref(),
            Some(b"second\n".as_slice())
        );
    }

    /// An offset that lands mid-character now returns exactly the raw bytes at
    /// that offset: the server no longer decodes, so it has no opinion on
    /// character boundaries. Re-aligning is the client's job.
    #[tokio::test]
    async fn an_offset_inside_a_character_returns_exact_bytes() {
        let _root = temp_root();
        append_build_output("p", 1, "unrecognized option \u{2018}-fno_char8_t\u{2019}\n")
            .await
            .unwrap();

        // 'unrecognized option ' is 20 bytes; the next character is a 3-byte
        // U+2018. Offset 21 is its second byte.
        let tail = read_build_output("p", 1, 21, None).await.unwrap().unwrap();
        assert_eq!(tail, b"\x80\x98-fno_char8_t\xE2\x80\x99\n");
    }

    /// A paged read returns exactly `limit` bytes, and the next page continues
    /// from the last byte returned.
    #[tokio::test]
    async fn a_paged_read_returns_at_most_the_requested_limit() {
        let _root = temp_root();
        append_build_output("p", 1, "first\nsecond\nthird\n")
            .await
            .unwrap();

        let first = read_build_output("p", 1, 0, Some(6))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first, b"first\n");

        let rest = read_build_output("p", 1, 6, Some(100))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(rest, b"second\nthird\n");
    }

    /// A limit past the end just returns the remainder; an offset at or past
    /// the end is an empty page, which is how a client knows it has caught up.
    #[tokio::test]
    async fn an_offset_at_or_after_the_end_is_an_empty_page() {
        let _root = temp_root();
        append_build_output("p", 1, "abc").await.unwrap();

        assert_eq!(
            read_build_output("p", 1, 2, None).await.unwrap().unwrap(),
            b"c"
        );
        assert!(
            read_build_output("p", 1, 3, None)
                .await
                .unwrap()
                .unwrap()
                .is_empty()
        );
        assert!(
            read_build_output("p", 1, 99, None)
                .await
                .unwrap()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn a_limit_is_clamped_to_the_maximum() {
        assert_eq!(resolved_limit(None), MAX_OUTPUT_BYTES);
        assert_eq!(resolved_limit(Some(10)), 10);
        assert_eq!(resolved_limit(Some(MAX_OUTPUT_BYTES)), MAX_OUTPUT_BYTES);
        assert_eq!(resolved_limit(Some(MAX_OUTPUT_BYTES + 1)), MAX_OUTPUT_BYTES);
    }

    #[tokio::test]
    async fn a_missing_log_is_none_rather_than_an_error() {
        let _root = temp_root();
        assert!(
            read_build_output("nope", 9, 0, None)
                .await
                .unwrap()
                .is_none()
        );
        assert!(build_log_size("nope", 9).await.is_none());
    }

    /// A pkgbase is a path segment, and for a git source it is whatever the
    /// PKGBUILD says rather than something the AUR validated.
    #[tokio::test]
    async fn a_hostile_pkgbase_stays_inside_the_root() {
        let root = temp_root();
        append_build_output("../../etc/passwd", 1, "nope\n")
            .await
            .unwrap();
        append_build_output("..", 1, "nope\n").await.unwrap();

        // The derived paths contain no traversal at all, which is the actual
        // property: asserting that some file outside the root does not exist
        // would pass or fail on what the host happens to have.
        for pkgbase in ["../../etc/passwd", "..", ".", "a/b"] {
            let path = build_log_path(pkgbase, 1);
            assert!(path.starts_with(root.dir.path()), "escaped: {path:?}");
            assert!(
                !path
                    .components()
                    .any(|c| c == std::path::Component::ParentDir),
                "traversal survived: {path:?}"
            );
        }
    }

    #[tokio::test]
    async fn removing_a_package_takes_all_of_its_logs() {
        let _root = temp_root();
        append_build_output("doomed", 1, "x\n").await.unwrap();
        append_build_output("doomed", 2, "y\n").await.unwrap();
        append_build_output("kept", 1, "z\n").await.unwrap();

        remove_package_logs("doomed").await;
        // Tolerates a package that never logged anything.
        remove_package_logs("never-built").await;

        assert!(
            read_build_output("doomed", 1, 0, None)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            read_build_output("doomed", 2, 0, None)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            read_build_output("kept", 1, 0, None)
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn removing_everything_clears_the_root() {
        let _root = temp_root();
        append_build_output("a", 1, "x\n").await.unwrap();
        append_build_output("b", 1, "y\n").await.unwrap();

        remove_all_logs().await;

        assert!(read_build_output("a", 1, 0, None).await.unwrap().is_none());
        assert!(read_build_output("b", 1, 0, None).await.unwrap().is_none());
    }
}
