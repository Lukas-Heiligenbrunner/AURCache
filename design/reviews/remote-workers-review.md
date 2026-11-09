# Review: Remote Build Workers rewrite

Scope: commits `28ff601` (Phase 1: DB changes) through `42c29c0` (Address
review comments) on `feature/abury/rewote-worker`, diffed against `4c4d672`
(base, before Phase 1). ~7,300 lines changed across 92 files, implementing the
architecture described in `design/implemented/remote-workers.md`.

This document records concrete, high-confidence issues found during review:
bugs, race conditions, resource leaks, and simplification opportunities.
Trivial style nits are omitted.

---

## High-confidence bugs

### 1. `requeue_or_fail` is not CAS-protected — can revert a completed build

**File:** `backend/aurcache-db/src/helpers/worker_jobs.rs:191` (called from
`heartbeat` at `:179` and `reap_expired_builds` at `:267`)

Every other status transition in this codebase (`claim_among`,
`record_built_version`, `complete_success`, `complete_failure`) uses a
compare-and-swap `UPDATE ... WHERE status = ACTIVE AND worker_id = ?` and
checks `rows_affected`, so a build that changed state between read and write
is never silently clobbered. `requeue_or_fail` breaks this pattern: it does a
plain `find_by_id` → `into_active_model()` → `update()` with **no filter on
the current `status`/`worker_id`** at write time.

Both call sites first select a snapshot of candidate ids (`status = ACTIVE
...`), then iterate and call `requeue_or_fail` afterward. If a worker's
`POST /worker/jobs/{id}/complete` races with the reaper/heartbeat pass — e.g.
the worker finishes and calls `complete_success` (which flips status to
`SUCCESS`, clears `worker_id`) in the gap between the reaper's `SELECT` and
its per-id `requeue_or_fail` call — `requeue_or_fail` will read the
now-`SUCCESS` row and unconditionally force it back to `ENQUEUED` (or
`FAILED` if the attempt budget is exhausted), bumping `attempt_count` even
though `packages.status` was already updated and dependents may have already
been promoted. This is a genuine correctness/data-integrity bug, not just a
benign duplicate-work race.

The code comment at `:265` ("Re-check ownership/state inside
`requeue_or_fail` via a fresh read...") is misleading — `requeue_or_fail`
re-reads the row but never re-checks it's still `ACTIVE`/owned before
writing.

**Fix:** Give `requeue_or_fail` a conditional update, e.g.
`Builds::update_many().filter(status.eq(ACTIVE)).filter(worker_id.eq(<expected>))...exec()`,
and treat `rows_affected == 0` as "already resolved, nothing to do" (mirroring
`record_built_version`/`complete_success`).

---

### 2. Worker mTLS listener uses a disconnected `SnapshotStore`

**File:** `backend/aurcache/src/main.rs:40-64`,
`backend/aurcache-api/src/init.rs:171-196` (`init_worker_api`)

`main.rs` creates one `Arc<SnapshotStore>` ("store") specifically because,
per its own comment, sharing it is required for correctness ("Its persistent
on-disk git checkouts and `refresh()` incremental-fetch model make this safe:
repeat requests reuse the same checkout instead of re-cloning/re-downloading").
That `store` is passed to `start_update_version_checking` and
`start_auto_update_job`, but **not** to `init_worker_api`. Inside
`init_worker_api`, a brand-new `SnapshotStore::new()` is constructed and
`.manage()`d for the Rocket instance backing `/worker/jobs/claim` (via
`worker.rs::claim_job` → `build_descriptor`, which takes `store:
&SnapshotStore`). `init_build_queue` was also changed to no longer accept a
store at all.

Two independent `SnapshotStore` instances now operate on the same on-disk
git checkout directories (same AUR package paths) without shared in-process
coordination/locking. Concurrent snapshot refreshes for the same package from
the worker-claim path and the version-check/auto-update path can race on the
same git checkout (partial checkouts, corrupted refs, or duplicate clone
attempts), and workers lose the "reuse instead of re-clone/re-fetch" benefit
the design relies on for correctness/perf.

**Fix:** Pass the same `Arc<SnapshotStore>` (or a clone of the underlying
store) into `init_worker_api` instead of constructing a new one; update the
stale `main.rs:40-43` comment ("shared across the build queue, version-check
loop, and auto-update job") once `init_build_queue` and `init_worker_api`'s
sharing scope is fixed to match reality.

---

### 3. Build workspace leaks on any non-success error path

**File:** `backend/aurcache-worker/src/job.rs:47-103` (`run_job_inner`)

Workdir cleanup (`remove_dir_all`) only runs at the very end of the happy
path, after `run_build`/`upload_artifacts` succeed. Every early return via
`?` — source download failure, config/chroot-prep failure
(`write_configs`, `ensure_base_chroot`), a build-spawn error, `run_build`'s
own failure, or `upload_artifacts` failing (no artifacts produced/read/upload
error) — bypasses that cleanup and leaves the extracted source tree and any
partially-built package artifacts on disk permanently. The only other
cleanup call (before extraction) only fires the *next* time the same
`build_id` is retried, but build ids are per-attempt, so a build that fails
once and is never retried with that exact id leaks its workdir forever. Over
time, on a worker handling many transient failures (network blips, bad
PKGBUILDs producing no artifacts, mirror timeouts), `WORKER_DATA_DIR/work/`
will grow unbounded and eventually fill the disk.

**Fix:** Wrap the body in a scope guard (e.g. `scopeguard::defer!` or a
`Drop` guard) that removes `workdir` unconditionally on function exit,
success or error.

---

### 4. Artifact publish (`ingest_pkgs`) happens before final lease re-verification

**File:** `backend/aurcache-api/src/worker.rs` (`complete_job`,
around line 449-490), `backend/aurcache-utils/src/repo_ingest.rs`

`complete_job` checks lease ownership once via `assert_owned_active` (a plain
read, not a CAS) at the very start of the handler, then runs the
(potentially slow, I/O-bound) `ingest_pkgs` — which writes files to the
shared repo tree, runs `repo_add`, and commits `files` table rows — *before*
`record_built_version`/`complete_success` perform their CAS checks. If the
lease is reaped (and the build reassigned/failed) while `ingest_pkgs` is in
flight, the stale worker's artifacts are still written into the live repo
and `files` table despite no longer legitimately owning the build; only the
final metadata update (`record_built_version`) discovers the lost lease and
errors out, by which point the repo mutation already happened and isn't
rolled back. Same-`pkg_id` overwrite semantics mitigate this somewhat, but a
build the server considers reclaimed can still race an artifact publish into
the live repo/db without any ownership guard around the publish step itself.

**Fix:** Either re-verify ownership immediately before/atomically with the
`files`-table writes in `ingest_pkgs` (e.g. pass through and check inside the
same short write transaction), or treat `ingest_pkgs`'s writes as
provisional/staged until `record_built_version`'s CAS succeeds.

---

## Medium-confidence issues

### 5. Cache eviction does a synchronous recursive directory walk inside an async task

**File:** `backend/aurcache-worker/src/cache.rs:98-141` (`Cache::evict` →
`scan_srcdest` → `dir_size`), called from `backend/aurcache-worker/src/job.rs:59`

`Cache::evict` recurses over the entire `srcdest` cache tree (up to
`cache_max_size`, default 20 GiB) using blocking `std::fs::read_dir`/
`metadata` calls, invoked directly (not via `tokio::task::spawn_blocking`) at
the start of every job ("Opportunistic cache GC"). With `concurrency`
defaulting to `nproc`, several jobs can trigger this walk concurrently, each
occupying a Tokio worker thread with blocking syscalls for as long as the
walk takes, starving other tasks scheduled on those threads (heartbeat, claim
polling, other builds' log/cancel polling).

**Fix:** Run `cache.evict(...)` inside `tokio::task::spawn_blocking`, or
throttle it (e.g. only scan periodically rather than at every job start).

---

### 6. Approve/revoke UI can misreport success on a non-200 "success" response

**File:** `frontend/lib/screens/workers_screen.dart` (`_run`),
`frontend/lib/api/workers.dart:13-19`

`approveWorker`/`revokeWorker` return `resp.statusCode == 200`, but `_run`
does `await action();` and unconditionally shows the success toast unless a
`DioException` is thrown. If the server ever responds with a non-200
"success" status (e.g. `202`/`204`, which Dio's default `validateStatus`
(`<300`) would not throw for), the API call returns `false` but the UI still
reports success and calls `ref.invalidate(listWorkersProvider)` as if it
worked, silently misinforming the operator that a worker was
approved/revoked. For a security-sensitive action (granting/revoking trust
for a build worker), this is a meaningful correctness/security concern.

**Fix:** Check the boolean returned by `action()` and only show the success
toast (and treat it as success) when it's `true`; show the failure toast
otherwise.

---

## Low-confidence / minor issues

### 7. Artifact filename interpolated unescaped into the request URL

**File:** `backend/aurcache-worker/src/client.rs:232-238` (`upload_artifact`)

`self.url(&format!("/jobs/{build_id}/artifacts/{filename}"))` embeds the
artifact filename directly as a URL path segment without percent-encoding.
`filename` comes from `build::discover_artifacts` / `is_artifact` in
`build.rs`, which only requires the name to contain `.pkg.tar`, not start
with `.`, and not end with `.sig` — it does not restrict to
makepkg-safe/URL-safe characters. A PKGBUILD that drops a file such as
`evil?x=1.pkg.tar.zst` or one containing `#` into the build directory will
have its `?`/`#` reinterpreted by the URL parser as a query
string/fragment separator rather than part of the path, silently truncating
or corrupting the upload request.

**Fix:** URL-encode the filename component (e.g.
`percent_encoding::utf8_percent_encode`) before interpolating it into the
path, and/or restrict `is_artifact` to only accept characters valid in real
makepkg-produced filenames.

---

## Areas reviewed with no reportable issues

- Worker claim CAS loop (`claim_among`), `record_built_version`,
  `complete_success`/`complete_failure` CAS guards, phased transactions in
  `ingest_pkgs_in`, and the `m20260818_000000_remote_workers` /
  `m20260508_000000_dependency_resolution_combined` migrations.
- mTLS/enrollment flow, cancellation/timeout handling in `runner.rs`/`job.rs`,
  and the cache-eviction sizing logic (`plan_eviction`) itself.
- `docker/entrypoint.sh`, `docker/nspawn-wrapper.sh`,
  `docker/worker-entrypoint.sh`, `docker/worker.Dockerfile`,
  `docker/Dockerfile`, `docker-compose.yaml`, `docker-compose.e2e.yaml`,
  `docker-compose.remote-worker.yaml`.
- `frontend/lib/models/worker.dart`, `frontend/lib/providers/workers.dart`,
  `frontend/lib/components/routing/router.dart`,
  `frontend/lib/components/routing/side_menu.dart`.

---

## Suggested priority for fixes

1. Issue 1 (`requeue_or_fail` CAS) — data-integrity bug, straightforward fix.
2. Issue 2 (disconnected `SnapshotStore`) — correctness/perf regression vs.
   design intent, straightforward fix (pass the existing `Arc` through).
3. Issue 3 (workdir leak) — operational risk (disk exhaustion), fix with a
   scope guard.
4. Issues 4-7 — lower urgency; worth follow-up but not blocking.
