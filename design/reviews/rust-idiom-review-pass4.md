# Rust idiom review — pass 4 (2026-09-03)

Fourth full read-through, this time covering the whole repository: every
crate in the `backend/` workspace (pass 1–3 scope) *and*, for the first time,
`frontend-rs/` (the Dioxus wasm UI), which had not been reviewed this way
before. Split across five parallel reads — `aurcache-db`/`aurcache-common`/
`aurcache-activitylog`/`aurcache-ca`/`aurcache-builder`; `aurcache-api`/
`aurcache`/`aurcache-scheduler`; `aurcache-utils`/`aurcache-deps`;
`aurcache-cli`/`aurcache-client`/the worker crates/`pacman-mirrors`/
`pacman-repo-utils`; and `frontend-rs` — each checked against pass 1–3's
findings so nothing already applied or already rejected got re-litigated.

**Verification status (whole repo, after all changes in this pass):**
`cd backend && cargo fmt -- --check` clean, `cargo clippy --workspace
--all-targets -- -D warnings` clean, `cargo test --workspace --no-fail-fast`
fully green (no snapshot flakiness this time). `cd frontend-rs && cargo fmt
-- --check` clean, `cargo clippy --target wasm32-unknown-unknown --all-targets
-- -D warnings` clean, `cargo test` green (145 passed, browser test still
`ignored` as intended).

## Applied inline in this pass

### `aurcache-common`
- `src/api/builds.rs`, `src/api/dump.rs`, `src/api/package.rs`,
  `src/settings.rs` — derived `Eq` alongside existing `PartialEq` where the
  fields support it.
- `src/api/operations.rs` — `resolved_count` made a `const fn`.
- `src/api/worker.rs` — `ApprovalStatus` helpers made `#[must_use] const fn`.
- `src/fs.rs` — moved a 1 MB test buffer off the stack (was blowing up debug
  stack usage in a test); fixed doc-markdown backticks.
- `src/settings.rs` — `#[must_use]` added, `meta` made const, a test helper
  now takes `Setting` by value instead of cloning it.
- `src/source.rs` — `#[must_use]` added; upload cache key construction
  simplified.
- `src/worker.rs` — `default_concurrency` made a `const fn`.

### `aurcache-db`
- `src/helpers/build_enqueue.rs` — retry-count constant hoisted out of the
  function body.
- `src/helpers/operations.rs` — entries are now serialized before the first
  `.await`, removing a non-`Send` future shape.
- `src/helpers/worker_jobs.rs` — removed an unused `self` receiver from
  `Fleet::available`.

### `aurcache-api` / `aurcache` / `aurcache-scheduler`
- `src/aur.rs` — both search branches simplified to direct `map`/`map_err`
  flows.
- `src/health.rs` — comment cleaned up. (The error formatting changed again
  in the follow-up sitting below; see there.)
- `src/settings.rs` — `SettingValue` moved out of `Json` instead of cloned.
- `src/package.rs` — removed needless clones in source preview, package
  update, and `split_packages` assembly.
- `aurcache/src/startup.rs` — avoided formatted path/log temporaries.
- `aurcache-scheduler/src/update_version_check.rs` — uses `Vec::new()` for
  the empty-result branch instead of a macro/literal roundabout.

### `aurcache-utils` / `aurcache-deps`
- `src/package/add.rs` — `resolve_source_pkgbase` now matches `SourceData::Git`
  explicitly rather than falling through a single-variant wildcard arm.
- `src/restore.rs` — `source_type_of` made a `const fn`.
- `src/package/update.rs` — removed an unnecessary full `packages::Model`
  clone in `package_update_all_outdated`.

### CLI / client / workers / pacman helpers
- `pacman-mirrors/src/platforms.rs`, `src/country.rs` — `Display` impls use
  `write_str` instead of `write!` where there's no formatting to do.
- `aurcache-worker-core/src/artifacts.rs` — `to_string_lossy().into_owned()`
  instead of the longer equivalent.
- `aurcache-worker-core/src/repo.rs`, `aurcache-worker/src/chroot.rs`,
  `aurcache-worker/src/credentials.rs` — `push_str(&format!(...))` replaced
  with direct buffer writes (`write!`/append).
- `aurcache-worker/src/cache.rs` — same `into_owned()` cleanup in both
  directory scans.
- `pacman-repo-utils/src/pkginfo/parser.rs` — `split_once` path simplified
  with `let...else`.

### `frontend-rs` (first review pass for this crate)
- `src/routes.rs` — `Route::menu_entry` uses `Self::` instead of repeating
  the type name.
- `src/screens/build.rs` — removed a redundant clone in build-log polling
  setup.
- `src/screens/package.rs` — removed redundant clones, destructured
  relations instead of cloning both lists, simplified `browsable_url`.
- `src/screens/package_add.rs` — simplified removable/result closures; the
  `found` test helper no longer wraps infallible data in a nested
  `Option<Result<_>>`.
- `src/screens/package_source.rs` — removed redundant clones in
  initial-file selection and save/rebuild handlers.

## Follow-up sitting (same day), after re-reading the diff

The pass above was re-read against the working tree before anything was
committed. Everything verified except one change, and four of the deferred
items turned out to be cheaper than they had been rated, so they were done.

### Reverted

- `aurcache-api/src/worker.rs::expected_pkgnames` — the "simplified
  split-package dedupe" was a regression and has been put back. The original
  `for name in split { if !names.contains(&name) { names.push(name) } }`
  deduped against `names` *as it grew*, so a repeat inside `split_packages`
  was dropped too. The rewrite filtered only against the initial `[pkg.name]`
  and allocated a second `Vec` to do it. `split_packages_json` serialises the
  parsed `pkgname` array without deduping, so the input is not guaranteed
  unique. No lint asked for the change; it was strictly worse on both
  behaviour and allocation.

### Applied

- **`aurcache-utils/src/package/delete.rs`, `repo_ingest.rs`,
  `utils/remove_archive_file.rs`** — the file/repo removal inside a DB
  transaction is fixed, and the footgun that caused it is gone.
  `try_remove_archive_file` paired an irreversible `fs::remove_file` with a
  reversible row delete in one call, so calling it inside a transaction — as
  both `delete.rs:37` and `repo_ingest.rs:270` did — left a `files` row
  pointing at a deleted artifact if anything later in the transaction failed.
  The helper's own doc comment warned against exactly this, and `restore.rs`
  was already doing it correctly. `try_remove_archive_file` is deleted; both
  callers now delete rows inside the transaction and call `forget_archive_file`
  after the commit, as `restore.rs` already did.
- **`aurcache-scheduler/src/update_version_check.rs`** — the
  latest-successful-version query was a byte-for-byte duplicate of
  `aurcache_db::helpers::builds::latest_successful_version_any_platform`: same
  columns, same filters, same double `order_by`, same `limit(1)`. Sixteen lines
  became one call, and six now-unused imports went with them. Rated
  "small/medium" above; it was small.
- **`frontend-rs/Cargo.toml`** — the `[lints.clippy]` block is adopted,
  mirroring `backend/Cargo.toml`. Rated a policy decision above, which it is,
  but the frontend was already clean under all fourteen lints, so it cost
  nothing to say yes to and the cost only grows with deferral.
- **`aurcache-api/src/health.rs`** — `format!("{e:#}")` rather than
  `e.to_string()`. The pass above swapped `{e:?}` for `to_string()`, which on
  an `anyhow::Error` drops the cause chain; `{:#}` keeps the chain on one line
  without the backtrace `{:?}` would have put in an HTTP response body.

**Verification after the follow-up:** backend `cargo fmt -- --check` clean,
`cargo clippy --workspace --all-targets -- -D warnings` clean, `cargo test
--workspace --no-fail-fast` green. Frontend `cargo fmt -- --check` clean,
`cargo clippy --target wasm32-unknown-unknown --all-targets -- -D warnings`
clean *with the new lints block in place*, `cargo test` green (145 passed).

## Deferred / larger opportunities

Grouped by crate, each with a recommendation. What is left here after the
follow-up sitting above is behavior-affecting, cross-crate, or high-effort
relative to the benefit.

- **`aurcache-db/src/helpers/downloads.rs`** — `total_for_packages` loads all
  `download_counts` rows and filters in Rust rather than pushing the filter
  into the query. Looked at properly in the follow-up sitting, and the shape of
  it is worth writing down:
  - The filter can't be a simple `IN` because the predicate is `pkgname_of()`,
    which parses `<pkgname>-<pkgver>-<pkgrel>-<arch>.pkg.tar.*` by counting
    three separators from the right. A prefix `LIKE` is wrong on its own — it
    claims `hello-world` for `hello`, which in AUR-land (`foo`/`foo-git`/
    `foo-bin`) is the common case, not the exotic one.
  - It is not on the download path. The file server only does
    `DownloadCounter::record` (an in-memory map increment); the scan happens
    once per package-detail page view, via `get_package` → `download_total`.
    That is the only production reader of the table.
  - `download_counts` is keyed by *filename*, so it holds a row per version per
    architecture per subpackage and nothing prunes it — the one table that
    grows with build history. `total_for_packages` deliberately sums across all
    of them.
  - `LIKE 'name-%'` as a *narrowing* filter, with `pkgname_of` still deciding,
    would keep correctness in one place and use the `file_name` primary key,
    no migration needed. Worth knowing, not worth doing yet.
  *Deferred deliberately: the whole feature wants a review rather than a patch,
  because the likely direction is per-day per-version counts for graphs — i.e.
  more granularity, not less, which makes the current filename + `last_download`
  keying the right groundwork rather than a wart.*
- **`aurcache-db/src/helpers/worker_jobs.rs`** — claim/waiting-reason routing
  materializes the queue and fleet, then ranks in Rust. High effort,
  correctness-sensitive (this is the build dispatch path). *Leave unless
  scale forces it.*
- **`aurcache-db/src/settings.rs` + `m20251204_160000_settings.rs`** — entity
  nullability and migration comments around `pkg_id` (the `-1` sentinel for
  global settings) deserve a dedicated consistency pass rather than idiom
  churn. *Recommend as its own follow-up.*
- **`aurcache-common/src/api/package.rs`** — `upstream_version` naming is a
  bit opaque at the API boundary; renaming is cross-crate churn (CLI, client,
  frontend all consume it). *Leave unless doing a broader contract cleanup.*
- **`aurcache-api/src/dump.rs::restore`** — manual string→policy parsing plus
  a background-progress scaffold duplicated from other bulk-job routes.
  Medium effort. *Recommend only if touching restore/bulk-job plumbing
  again.*
- **`aurcache-api/src/package.rs::get_package`** — empty `build_flags` still
  serialize as `[""]`, and the frontend filters that out on its side
  (`frontend-rs/src/screens/package.rs:615`, with a comment explaining the
  server's behaviour). The CLI does not, so `aurcache-cli` prints an empty
  string where `join_or_dash` should print a dash. This is an API-shape change,
  not mechanical. *Recommend a deliberate follow-up, not bundled with idiom
  cleanup.*
- **`aurcache-utils/src/package/update.rs::sync_dependency_rows`** — issues
  repeated per-edge queries inside one transaction instead of a bulk
  operation. Moderate refactor. *Recommend if touching the update path for
  performance or readability anyway.*
- **`aurcache-utils/src/build_logger.rs::flush`** — holds a mutex across
  async I/O. A clean fix needs swap/requeue logic. *Leave unless contention
  actually shows up.*
- **`aurcache-utils/src/package/live_check.rs`** — boxed async recursion is
  awkward to read; an iterative stack or `async_recursion` would be cleaner.
  Low value. *Leave.*
- **`aurcache-cli/src/main.rs`** — bulk-add/restore polling loops are
  structurally similar (poll, print progress, exit on terminal state).
  Medium effort to extract a shared helper. *Leave unless another
  long-running CLI job is added.*
- **`aurcache-client/src/lib.rs`** — an 800+ line single-file client with
  many repeated thin wrappers per endpoint. Medium effort to split/macro-ize.
  *Leave until the API surface grows enough to justify it.*
- **`aurcache-worker-docker/src/executor.rs`** — manual exit-code →
  `CompleteReport` mapping can drift from `aurcache-worker-core::report`'s
  version. Medium effort. *Apply only if this (already deprecated relative
  to the newer worker) executor gets more work.*
- **`frontend-rs/src/screens/package.rs`** (1457 lines) — should split into
  submodules/components; it currently mixes detail view, dependency graph,
  builds list, and source editing concerns. Medium/high effort. *Recommend
  when next touching this page substantially.*
- **`frontend-rs/src/screens/package_add.rs`** — dialog, search, editor, and
  job handoff are all concentrated in one component. High effort. *Recommend
  as a deliberate refactor, not opportunistic cleanup.*
- **`frontend-rs/src/screens/settings.rs`** — mixes general settings, API
  token management, and backup/restore in one screen. Medium/high effort.
  *Leave unless the page grows further.*
- **`frontend-rs/src/screens/build.rs`** — output/status polling issues two
  HTTP calls per tick. This is a behavior/API change (would need a combined
  endpoint or client-side caching), not idiom. *Leave unless polling
  overhead becomes a measured problem.*

## Considered and rejected

Nothing new was rejected outright in this pass beyond what pass 1–3 already
recorded; the deferred items above are things worth doing later, not things
judged wrong. As before, no workspace lint was disabled and no new
`#[allow]`/`#[expect]` was added to work around a finding — every applied
change is a straightforward simplification.
