# Build-scoped log entries

`design/implemented/structured-logs.md` gives every log row an optional
*scope*, an entity the row was emitted during, so that `build:hello/7` gathers
the events *about* a build and the ones that happened *while* it ran. Most of
the mechanism was built with the structured log. Nothing uses it yet: no
production code emits through a scoped handle, and no page asks for a build's
entries. This doc covers what is left.

Status: **Proposed** · Last updated: 2026-10-06

---

## What exists

- **The handle.** `ActivityLog::scoped(entity)`
  (`backend/aurcache-activitylog/src/activity_utils.rs:161`) returns a copy of
  the handle that stamps every entry it emits with that scope.
- **Storage and index.** The scope is written to `log.scope` and indexed in
  `log_entity` under its own role, `SCOPE_ROLE` (`activity_utils.rs:69`,
  `:209`). So the log's existing entity filter finds a scoped row like any
  other reference, and no query has to know scopes exist.
- **Reading.** `LogStore` parses the scope back onto the entry
  (`log_store.rs:175`), and the Logs page shows it as one more entity chip on
  the row (`frontend-rs/src/screens/logs.rs:677`).
- **Tests.** `a_scope_is_indexed_like_any_other_reference`
  (`backend/aurcache-activitylog/tests/structured_log.rs:212`) covers emitting,
  indexing and filtering.

## What is missing

### 1. Nothing emits under a scope

Most events on a build's path already name the build in their payload
(`BuildStarted`, `BuildSucceeded`, `BuildPublished`, `PublishFailed`,
`BuildRecordFailed`, and `WorkerWarning`/`WorkerError` when the worker says
which job). Those are found by the entity filter already, and scoping them
would add nothing.

The ones that would gain from a scope happen during a build without naming it:

- **Repository commits while publishing.** `publish_build`
  (`backend/aurcache-utils/src/publish.rs:38`) goes through `Repository`, whose
  `RepoCommitRetried` and `RepoFileFailed` name a path or an attempt, never the
  build. `Repository` keeps the handle it was built with in `Services`, so a
  scope has to reach it through `begin()` (`repository.rs:432`). Either
  `begin()` takes the handle to log through, or the `Update` it returns
  carries one (`update.scoped(build)`).
- **Dependents that fail to trigger.** `DependentsTriggerFailed` names the
  package (`publish.rs:97`). Emitting it through a handle scoped to the
  published build would put it on that build's history, next to the
  `BuildUnblocked` entries that already carry `by`.

### 2. No page asks for a build's entries

The package and worker pages each show a `RecentActivity` panel
(`frontend-rs/src/screens/logs.rs:313`; used at `package.rs:204` and
`worker.rs:234`). The build page (`frontend-rs/src/screens/build.rs`) shows
only the build's output. A `RecentActivity { about: EntityRef::Build(..) }`
panel there is the reader the scope was built for. Without it, scoped rows
only appear on the Logs page under a hand-typed filter.

### 3. Threading the handle, if it gets deep

Following the structured-logs design, the scope travels explicitly, through
the handle. The publish path is one function and a `Repository` call, so this
is cheap. If a later scoped path runs down a deep call stack, the planned
alternative still applies: a `tokio::task_local` scope that `emit` reads when
the handle has none. It is a drop-in change to `emit` that leaves every call
site alone. Nothing needs it yet.

## Order

1. Add the build page's `RecentActivity` panel: useful on its own, since it
   shows what the payloads already name, and it gives (2) a place to show up.
2. Scope the publish path: pass a scoped handle into `Repository::begin`, and
   emit `DependentsTriggerFailed` through it.
3. Tests: a publish whose repository commit retries puts the retry on the
   build's history (`backend/aurcache-utils/tests/publish.rs` already
   publishes through a real `Repository`). The page is covered by
   `scripts/test-frontend.sh` once `/package/<pkgbase>/build/<number>` asserts
   that the panel mounts.

## Not doing

- **Scoping events that already name the build.** The scope would duplicate
  the payload's reference under another role.
- **A `tracing` layer that infers scope from spans.** As the structured-logs
  design says, this is the one option that needs a dynamic round trip, and
  the explicit handle covers every case here.
