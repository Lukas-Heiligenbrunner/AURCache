//! The build flags page, held to the list a package chooses from.

use aurcache_common::build_flags::OFFERED;
use std::path::Path;

/// One row of the page's table.
#[derive(Debug, PartialEq, Eq)]
struct Row {
    flag: String,
    summary: String,
}

fn page() -> String {
    let page =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/docs/Configuration/build-flags.md");
    std::fs::read_to_string(page).expect("the build flags page")
}

/// The table's rows whose first cell is a flag in backticks.
fn rows(page: &str) -> Vec<Row> {
    page.lines()
        .filter_map(|line| {
            let mut cells = line.strip_prefix('|')?.split('|').map(str::trim);
            let flag = cells.next()?.strip_prefix('`')?.strip_suffix('`')?;
            let summary = cells.next()?;
            Some(Row {
                flag: flag.to_string(),
                summary: summary.to_string(),
            })
        })
        .collect()
}

/// Every offered flag is on the page, in order, with its summary, and
/// nothing else is in the table.
#[test]
fn the_page_lists_the_offered_flags() {
    let offered: Vec<Row> = OFFERED
        .iter()
        .map(|o| Row {
            flag: o.flag.to_string(),
            summary: o.summary.to_string(),
        })
        .collect();
    assert_eq!(rows(&page()), offered);
}
