//! How results are shown: tables, JSON, and the small formatting helpers
//! every command shares.

use crate::cli::OutputFormat;
use anyhow::{Context, Result};
use aurcache_client::{GraphDataPoint, ListStats, SearchResult, ServerInfo, UserInfo};
use serde::Serialize;
use serde_json::Value;

pub(crate) fn print_done_message(format: OutputFormat, message: &str) {
    if format == OutputFormat::Text {
        println!("{message}");
    }
}

/// Print `value` as JSON, or hand it to `print_text` for the human-readable form.
pub(crate) fn render<T: Serialize>(
    format: OutputFormat,
    value: &T,
    print_text: impl FnOnce(&T),
) -> Result<()> {
    match format {
        OutputFormat::Json => print_json(value),
        OutputFormat::Text => {
            print_text(value);
            Ok(())
        }
    }
}

pub(crate) fn print_json<T: Serialize>(value: &T) -> Result<()> {
    println!(
        "{}",
        serde_json::to_string_pretty(value).context("failed to serialize JSON output")?
    );
    Ok(())
}

/// How often the bulk-add/restore followers poll progress: one number so
/// tuning it tunes every follower, human and machine output alike.
pub(crate) const PROGRESS_POLL_INTERVAL_SECS: u64 = 1;

/// Pad `cell` to `width` terminal cells. `format!("{cell:<width$}")` counts
/// bytes, so a CJK package name would throw every column after it off by one
/// per wide character.
pub(crate) fn pad_cell(cell: &str, width: usize) -> String {
    use unicode_width::UnicodeWidthStr;
    let mut padded = cell.to_string();
    padded.extend(std::iter::repeat_n(' ', width.saturating_sub(cell.width())));
    padded
}

pub(crate) fn print_table(headers: &[&str], rows: &[Vec<String>]) {
    use unicode_width::UnicodeWidthStr;
    // Sized by the widest row, not just the headers: a row longer than
    // `headers` must print, not panic the CLI with an index-out-of-bounds.
    // Widths are terminal cells, not bytes, or wide names misalign the table.
    let columns = headers
        .len()
        .max(rows.iter().map(Vec::len).max().unwrap_or(0));
    let mut widths = vec![0; columns];
    for (index, header) in headers.iter().enumerate() {
        widths[index] = header.width();
    }
    for row in rows {
        for (index, cell) in row.iter().enumerate() {
            widths[index] = widths[index].max(cell.width());
        }
    }

    let header = headers
        .iter()
        .enumerate()
        .map(|(index, value)| pad_cell(value, widths[index]))
        .collect::<Vec<_>>()
        .join("  ");
    println!("{header}");

    let separator = widths
        .iter()
        .map(|width| "-".repeat(*width))
        .collect::<Vec<_>>()
        .join("  ");
    println!("{separator}");

    for row in rows {
        println!(
            "{}",
            row.iter()
                .enumerate()
                .map(|(index, cell)| pad_cell(cell, widths[index]))
                .collect::<Vec<_>>()
                .join("  ")
        );
    }
}

/// The one-line health report: the server is up, and this is what is running.
/// Named as the server's because the CLI has a version of its own (`--version`).
pub(crate) fn format_health(info: &ServerInfo) -> String {
    format!("ok (server {})", info.version)
}

pub(crate) fn print_health(info: &ServerInfo) {
    println!("{}", format_health(info));
}

pub(crate) fn print_user_info(user: &UserInfo) {
    println!(
        "username: {}",
        user.username
            .as_deref()
            .unwrap_or("(authentication disabled)")
    );
    println!("has_api_token: {}", user.has_api_token);
}

pub(crate) fn print_stats(stats: &ListStats) {
    println!("total_builds: {}", stats.total_builds);
    println!("successful_builds: {}", stats.successful_builds);
    println!("failed_builds: {}", stats.failed_builds);
    println!("recent_builds: {}", stats.recent_builds);
    println!("recent_successful: {}", stats.recent_successful);
    println!("recent_failed: {}", stats.recent_failed);
    println!(
        "avg_build_time_seconds: {}",
        stats
            .avg_build_time
            .map_or_else(|| "unknown".to_string(), |seconds| seconds.to_string())
    );
    println!(
        "repo_size_bytes: {}",
        stats
            .repo_size
            .map_or_else(|| "-".to_string(), |bytes| bytes.to_string())
    );
    println!("requested_packages: {}", stats.requested_packages);
    println!("dependency_packages: {}", stats.dependency_packages);
    println!("total_build_trend: {:.2}", stats.total_build_trend);
    println!("avg_build_time_trend: {:.2}", stats.avg_build_time_trend);
}

pub(crate) fn print_graph(points: &[GraphDataPoint]) {
    let rows = points
        .iter()
        .map(|point| {
            vec![
                point.year.to_string(),
                format!("{:02}", point.month),
                point.count.to_string(),
                point.successful.to_string(),
            ]
        })
        .collect::<Vec<_>>();
    print_table(&["year", "month", "count", "successful"], &rows);
}

pub(crate) fn print_search_results(results: &[SearchResult]) {
    let rows = results
        .iter()
        .map(|result| vec![result.name.clone(), result.version.clone()])
        .collect::<Vec<_>>();
    print_table(&["name", "version"], &rows);
}

pub(crate) fn print_raw_response(text: &str) -> Result<()> {
    if let Ok(value) = serde_json::from_str::<Value>(text) {
        println!(
            "{}",
            serde_json::to_string_pretty(&value).context("failed to pretty-print JSON")?
        );
    } else {
        print!("{text}");
    }
    Ok(())
}

/// Where a watch's progress lines go.
///
/// Machine output owns stdout: a `--format json` caller is parsing one
/// document, so progress goes to stderr rather than interleaving text into it.
#[derive(Copy, Clone, Debug)]
pub(crate) struct Progress(pub(crate) OutputFormat);

impl Progress {
    pub(crate) fn line(self, text: &str) {
        if self.0 == OutputFormat::Json {
            eprintln!("{text}");
        } else {
            println!("{text}");
        }
    }
}

pub(crate) fn bool_label(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

pub(crate) fn option_text(value: Option<&str>) -> String {
    value
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .unwrap_or_else(|| "-".to_string())
}

pub(crate) fn join_or_dash<S: AsRef<str>>(values: &[S]) -> String {
    if values.is_empty() {
        "-".to_string()
    } else {
        values
            .iter()
            .map(AsRef::as_ref)
            .collect::<Vec<_>>()
            .join(", ")
    }
}

pub(crate) fn format_timestamp(timestamp: Option<i64>) -> String {
    timestamp
        .and_then(|timestamp| jiff::Timestamp::from_second(timestamp).ok())
        // RFC 3339 in the local offset, without jiff's `[zone]` suffix.
        .map(|timestamp| {
            let local = timestamp.to_zoned(jiff::tz::TimeZone::system());
            timestamp.display_with_offset(local.offset()).to_string()
        })
        .unwrap_or_else(|| "-".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Table cells pad to terminal cells, not bytes: two CJK characters are
    /// four cells wide, so padding to six leaves two spaces — a byte count
    /// would see six bytes already and leave none.
    #[test]
    fn pad_cell_counts_cells_not_bytes() {
        assert_eq!(pad_cell("ab", 4), "ab  ");
        assert_eq!(pad_cell("日本", 6), "日本  ");
    }

    /// `health` reports the running server's version, not just that it is up —
    /// and names it as the server's, because the CLI has a version of its own.
    #[test]
    fn health_reports_the_server_version() {
        let info = super::ServerInfo {
            version: "0.5.0+g8afa04a.dirty".to_string(),
            timezone: None,
        };
        assert_eq!(format_health(&info), "ok (server 0.5.0+g8afa04a.dirty)");
    }
}
