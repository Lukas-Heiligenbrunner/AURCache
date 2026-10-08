# Repository review and implementation — 2026-10-08

All ten larger changes from the review are implemented, following approval.
The work keeps each invariant in its owning crate and replaces ambiguous query
parameters and action results with named types. Unreleased callers were migrated
rather than kept behind compatibility wrappers.

## Scope

The review covered all 19 backend crates, the Rust frontend, build and packaging
helpers, container configuration, CI and the documentation site's configuration.
The graph inventory, production signatures, boolean parameters/results, tuple
results, panic sites and large functions were surveyed. Detailed implementation
review concentrated on publication, deletion, queue transitions, dependency
resolution, authentication, settings, storage accounting and frontend polling.

This is a repository-wide structural review with targeted source inspection;
it does not establish that every source line or platform-specific failure is
correct. Upgrades from the released upstream 0.5 schema remain supported.

## Small changes made

- Preserve publication staging when loading the build fails. Only a confirmed
  missing build permits cleanup. A closed-database regression test covers this.
- Ignore unfinished `.part` artifacts during worker discovery, matching cache
  selection. The existing discovery test now includes an unfinished file.
- Parse the final `.pkg.tar.` filename delimiter, so a package named
  `my.pkg.tar.tool` is not truncated. Added a regression case.
- Reject corrupt archive members instead of logging the error and publishing
  incomplete repository metadata. A regression archive has valid `.PKGINFO`
  followed by an invalid member header.
- Return discarded bytes from `append_capped`, replacing an action's boolean
  result and consolidating the two drain paths. Updated its caller and tests.
- Name the cgroup unset-limit policy `UnsetLimit::{Preserve, Reset}` rather than
  passing unexplained `true`/`false` arguments.
- Represent config-file save feedback as `Result<String, String>` instead of a
  message/boolean tuple.
- Remove unused dependencies; make `rust-embed` belong to the `static` feature.
  The corresponding lockfile changes are dependency removal, not an upgrade.
- Remove CI's obsolete placeholder web assets: the static build script now
  builds and installs the actual frontend before embedding it.
- Read each package's version from its own AUR submodule when building source
  archives, matching the container packaging path.
- Remove an unused SSH-test variable, separate an export from its fallible
  assignment, and identify the sourced packaging helper's shell to ShellCheck.

## Larger changes implemented

### 1. Durable repository finalization

[Repository](../backend/aurcache-utils/src/repository.rs) now owns the SQL
transaction as well as preparation and finalization. Its callback receives a
`DatabaseTransaction`; publication, deletion and restore participate in that
transaction rather than committing their own.

A [migration](../backend/aurcache-db/src/migration/m20261008_000001_repository_updates.rs)
adds the singleton `repository_updates` journal. The caller's SQL changes and
filesystem plan commit together. Fallible artifact and database renames replay
idempotently; the journal is cleared only after all replacements finish. Startup,
the next update and repository sweeping recover pending work before proceeding.
Blocking filesystem tasks retain an owned lock through cancellation.

[Publication](../backend/aurcache-utils/src/publish.rs) retains uploaded originals
until finalization, recording success and dependent promotion finish. A restart
also resumes successful builds that still have an upload directory, covering a
crash between success and dependent promotion. Post-commit failures preserve the
recoverable state instead of failing the build or discarding its uploads.

Regression tests cover each rename failure, restart and repeated recovery,
blocked sweeping, cancellation during actual blocking preparation, and failed
or interrupted dependent promotion.

### 2. Atomic build and package status changes

Queue insertion, promotion, demotion and claiming open their own transactions,
including `refresh_package_status`. Existing caller transactions use savepoints.
Package status derivation locks the package row on PostgreSQL so concurrent
refreshes cannot overwrite a newer aggregate with an older one. Cancellation,
completion, abandonment and publication retain their transaction boundaries.

Fault tests reject package-status updates and verify each queue mutation and
claim rolls back. Existing claim races and dependency transition tests remain.

### 3. One persisted CA identity

[Ca](../backend/aurcache-ca/src/lib.rs) validates the certificate, CA constraint
and certificate/key match when loading. An empty directory can create an identity;
a partial or invalid legacy pair fails without replacing the surviving material.

The private, atomically replaced `ca-identity.json` is authoritative. PEM files
are repairable exports of that pair, so interruption between exports does not
rotate the identity. Explicit `restore` and `rotate` operations persist the pair;
backup reads it together through `Ca::load`.

Tests cover partial, malformed and mismatched identities and recovery after
interrupted PEM installation.

### 4. Explicit directory measurement completeness

[DirectoryMeasurement](../backend/aurcache-common/src/fs.rs) is the shared tree
measurement result: `Complete { bytes }` or `Incomplete { known_bytes }`.
It walks iteratively and skips symlink entries. Worker sizing first uses it,
then attempts privileged measurement. A failed total remains `None` in telemetry
and size stamps.

Cache eviction treats unknown sizes conservatively when a budget is enabled,
while protecting active entries. Disabled budgets remain disabled. Tests cover
unreadable trees, missing roots, symlinks, deep nesting and incomplete-size eviction.

### 5. Owned worker job phases

[Job execution](../backend/aurcache-worker/src/job.rs) now separates preparation,
bind planning, process execution, execution reporting, lease settlement and cache
cleanup. `PreparedJob` owns its resources; `BuildExecution` carries the process
outcome and measurements needed for reporting. The top-level job function
orchestrates these phases.

Private package/keyring cleanup is attached to an owned guard. The chroot lease
counter also has an owned guard through acquisition and release, preventing an
aborted future from permanently occupying capacity. Cancellation tests cover
both guards; the kernel and full worker suites cover storage and process behavior.

### 6. Shared setting mutation and draft rules

[Setting editing](../frontend-rs/src/setting_editor.rs) provides `SettingScope`,
`SettingChange`, `SettingEdit`, `EditorDraft` and one mutation controller. Global,
package and config-file controls use it; the package build settings share one
settings resource.

Refetch seeds clean drafts and preserves dirty ones. Successful mutations
acknowledge the submitted value, preserving newer edits made during a request.
Busy state blocks duplicate submission; failed resets preserve the override.
Pure tests cover draft reseeding, and a browser regression delays a save,
double-clicks it, edits during the request and rejects a reset.

### 7. Build-log polling separated from presentation

[use_build_log](../frontend-rs/src/build_log.rs) owns polling and the named
controller state; the component renders it. [LogCursor](../frontend-rs/src/log_tail.rs)
centralizes offsets, restart positions and UTF-8 advancement. Named progress
results distinguish catch-up, more output and retry.

Terminal builds keep retrying failed log reads until the final page is fetched.
Route identity remounts the controller. Tests cover UTF-8 tails, truncation,
backlog jumps and retries; existing browser interactions cover following controls.

### 8. Canonical client queries and build identity

The client exposes only `list_builds(&BuildQuery)` and
`list_packages(&PackageQuery)`. `DependencySelection` names the package listing
policy. All CLI and frontend callers use those query types.

The CLI uses the shared `BuildRef`, with explicit `parse_id`/`id` methods for
`pkgbase/number`; log references keep their namespaced parser and formatter.
CLI parsing and existing client/protocol tests cover the migrated callers.

### 9. Central elapsed-time validity

`BuildSummary::elapsed` uses the shared checked `elapsed_between` helper.
Formatting, sorting, median selection and previous-build comparisons consume
validated durations. One SQL expression applies the same validity rules to
statistics, trends and longest-build ordering, guarding PostgreSQL arithmetic.

Average build time is `Option<u64>`: no measurement is unknown, while zero is a
real duration. Previous durations are nonnegative values too. Tests cover missing,
zero, reversed and overflowing intervals and ranking invalid values. A PostgreSQL
smoke test exercises the actual aggregate and journal queries.

### 10. Shared shell quoting and private writes

[quote_argument](../backend/aurcache-common/src/shell.rs) is the dependency-free
POSIX shell argument helper used by the CLI and both workers.
[write_private](../backend/aurcache-common/src/fs.rs) creates a private sibling,
syncs it, atomically replaces the destination and syncs the parent. CA persistence,
worker identity, CLI configuration and private dumps use it.

Tests pass hostile arguments through a real shell and verify replacement modes,
failed replacement and temporary-file cleanup.

## Validation

- Backend `cargo test --all`: 1,021 passed. Browser/kernel/PostgreSQL opt-in
  tests are run separately below.
- Frontend `cargo test`: 249 passed.
- Backend Clippy over all targets, frontend Clippy over host and wasm targets,
  and API Clippy with `static`: passed with warnings denied.
- `cargo check -p aurcache-common --no-default-features`: passed.
- `scripts/test-frontend.sh`: all 122 routes rendered; interactions passed,
  including the delayed save, duplicate submission and failed reset regression.
- `scripts/test-kernel.sh`: all 23 privileged tests passed. The three worker
  targets (pool drain, cache volumes and cgroup measurements) were rerun after
  the lease ownership change and passed.
- `scripts/test-e2e.sh hello 18080`: split server/worker enrollment, build,
  upload, publication and installation from the repository passed (379 seconds).
- `postgres_elapsed_queue_and_repository_journal`: passed against an empty
  disposable PostgreSQL 17 database; fresh migrations, duration validity, queue
  transactions and journal commit/replay were exercised.
- Formatting and `git diff --check`: passed.
- Shell syntax, ShellCheck and mocked packaging-version checks passed during
  the small-fix review. ShellCheck retains the existing unquoted package-list
  expansion warning in `docker/add-aur.sh`.

The PostgreSQL smoke test is opt-in:
`AURCACHE_TEST_POSTGRES_URL=... cargo test -p aurcache-api --lib postgres_elapsed_queue_and_repository_journal -- --ignored`.
It expects an empty disposable database and applies migrations there.

## Practical limits

The runtime journal stores repository paths, so the database and repository root
must be moved or restored together while an update is pending. A pending plan
that still cannot finalize blocks further repository mutations and sweeping;
its filesystem error must be repaired. Restart resumes pending publication and
dependent promotion. Existing worker trust is preserved on incomplete CA writes;
explicit CA rotation still requires replacing worker certificates.

Cross-architecture image builds, native package installation on the host, ZFS
and block-device/zvol pool configurations were not exercised. The full worker
smoke test installs the built package inside a disposable Arch container.
