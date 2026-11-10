//! The server's environment, held to its one declaration in `server_env`.

use aurcache_common::server_env;
use aurcache_common::settings::Setting;
use std::path::{Path, PathBuf};

/// The crates the server is built from. Workers and the CLI declare their own.
const SERVER_CRATES: &[&str] = &[
    "aurcache",
    "aurcache-activitylog",
    "aurcache-api",
    "aurcache-ca",
    "aurcache-common",
    "aurcache-db",
    "aurcache-deps",
    "aurcache-scheduler",
    "aurcache-utils",
    "pacman-mirrors",
    "pacman-repo-utils",
];

/// Where a name is spelled out on purpose: the declarations themselves.
const DECLARATIONS: &[&str] = &[
    "aurcache-common/src/server_env.rs",
    "aurcache-common/src/settings.rs",
];

fn backend() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
}

/// Every name the server reads: the table, and the settings' variables.
fn declared() -> Vec<&'static str> {
    server_env::ALL
        .iter()
        .map(|var| var.name())
        .chain(Setting::ALL.iter().filter_map(|s| s.meta().env_name))
        .collect()
}

#[test]
fn every_variable_is_on_the_environment_page() {
    let page = backend().join("../docs/docs/Configuration/environment-variables.md");
    let page = std::fs::read_to_string(&page).expect("the environment page");
    let missing: Vec<_> = declared()
        .into_iter()
        .filter(|name| !mentions(&page, name))
        .collect();
    assert!(
        missing.is_empty(),
        "not on the environment page: {missing:?}"
    );
}

/// The other way round: a row for a variable nothing reads any more is a
/// setting that silently does nothing.
#[test]
fn every_row_on_the_environment_page_is_declared() {
    let page = backend().join("../docs/docs/Configuration/environment-variables.md");
    let page = std::fs::read_to_string(&page).expect("the environment page");
    let declared = declared();
    let stale: Vec<_> = page
        .lines()
        .filter_map(|line| line.strip_prefix('|')?.split('|').next())
        .map(str::trim)
        .filter(|cell| {
            !cell.is_empty()
                && cell
                    .chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        })
        .filter(|name| !declared.contains(name))
        .collect();
    assert!(stale.is_empty(), "documented but not declared: {stale:?}");
}

#[test]
fn the_table_names_each_variable_once() {
    let mut names = declared();
    names.sort_unstable();
    let before = names.len();
    names.dedup();
    assert_eq!(names.len(), before, "a variable is declared twice");
}

/// A read by a string of its own is one the table, and so the documentation
/// test, does not know about. Only production code is held to it: a test may
/// set a variable under its plain name.
///
/// This is a check on the usual spellings -- a declared name in a literal, and
/// `env::var("...")`/`env::var_os("...")` -- not a proof: a name built at run
/// time or read through an alias gets past it.
#[test]
fn the_server_reads_variables_only_through_the_table() {
    let declared = declared();
    let mut found = Vec::new();
    for krate in SERVER_CRATES {
        for file in rust_files(&backend().join(krate).join("src")) {
            let relative = file
                .strip_prefix(backend())
                .unwrap()
                .to_string_lossy()
                .into_owned();
            if DECLARATIONS.iter().any(|d| relative == *d) || relative.contains("/tests") {
                continue;
            }
            let source = std::fs::read_to_string(&file).unwrap();
            for (index, line) in production(&source).enumerate() {
                let code = line.split("//").next().unwrap();
                let named = string_literals(code).filter(|literal| declared.contains(literal));
                let read = env_reads(code).filter(|literal| !declared.contains(literal));
                for literal in named.chain(read) {
                    found.push(format!("{relative}:{}: \"{literal}\"", index + 1));
                }
            }
        }
    }
    assert!(
        found.is_empty(),
        "read through aurcache_common::server_env instead:\n{}",
        found.join("\n")
    );
}

/// The lines of `source` before its test module. An item marked
/// `#[cfg(test)]` on its own is skipped past, not taken for the module.
fn production(source: &str) -> impl Iterator<Item = &str> {
    let lines: Vec<&str> = source.lines().collect();
    let module = |line: &str| line.split_whitespace().take(2).any(|word| word == "mod");
    let end = lines
        .windows(2)
        .position(|pair| pair[0].trim() == "#[cfg(test)]" && module(pair[1]))
        .unwrap_or(lines.len());
    lines.into_iter().take(end)
}

/// Whether `text` names `name` as a word of its own, not as part of a longer
/// variable.
fn mentions(text: &str, name: &str) -> bool {
    text.match_indices(name).any(|(at, _)| {
        let word = |c: char| c.is_ascii_alphanumeric() || c == '_';
        let before = text[..at].chars().next_back().is_none_or(|c| !word(c));
        let after = text[at + name.len()..]
            .chars()
            .next()
            .is_none_or(|c| !word(c));
        before && after
    })
}

/// The name in each `env::var("...")` or `env::var_os("...")` on a line.
fn env_reads(line: &str) -> impl Iterator<Item = &str> {
    ["env::var(\"", "env::var_os(\""]
        .into_iter()
        .flat_map(move |call| {
            line.match_indices(call)
                .filter_map(move |(at, _)| line[at + call.len()..].split('"').next())
        })
}

/// The contents of each plain `"..."` literal on a line.
fn string_literals(line: &str) -> impl Iterator<Item = &str> {
    line.split('"').skip(1).step_by(2)
}

fn rust_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return files;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            files.extend(rust_files(&path));
        } else if path.extension().is_some_and(|e| e == "rs") {
            files.push(path);
        }
    }
    files
}
