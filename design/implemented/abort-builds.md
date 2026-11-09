# Design: Aborting a running build

Status: **Implemented** · Last updated: 2026-09-22

Extends [`remote-workers.md`](remote-workers.md). The worker is pull-only; it
connects out to the server and the server never reaches into a worker. This
design gives the server a way to *stop* a build it no longer wants running, by
riding the existing heartbeat — the only channel that already goes back — and it
revises `remote-workers.md`'s §"Build liveness & failure handling": an
"abandoned" build is failed for good and a *fresh* build row is queued in its
place, replacing the bounded `attempt_count` requeue of the same row. A build
row is one attempt,
never more.

## Motivation

A build that must stop cannot currently be stopped:

- `POST /package/<pkgbase>/build/<number>/cancel` sends `Action::Cancel`, and
  the coordinator (`backend/aurcache-builder/src/init.rs::cancel_build`) moves
  the row to terminal `FAILED`, **clearing `worker_id`** while doing so — but
  the worker's cancel poll `GET /worker/jobs/<id>/status`
  (`backend/aurcache-api/src/worker.rs:808-826`) returns **403 to anyone who is
  not the owner** — and after a cancel there *is* no owner. `remote_cancel`
  (`backend/aurcache-worker-core/src/protocol.rs:21`) is `is_ok_and(...)`, so a
  403 reads as "no cancel". The worker can never observe the cancel. Its own
  comments (`builder/init.rs:53-55`) describe a signal path the code contradicts.
- Even if it were signalled, the kill misses: the chroot executor stops the
  build with `child.start_kill()` (`backend/aurcache-worker/src/job.rs:555`) —
  SIGKILL on `sudo` only. The tree (`makechrootpkg` → `arch-nspawn` →
  `makepkg` → compilers) survives and builds on.
- The server's "this build has gone on too long" machinery — the lease reaper
  (`backend/aurcache-scheduler/src/lease_reaper.rs`) requeueing a build past
  `JOB_TIMEOUT` + a 300 s backstop grace via `requeue_or_fail` — requeues the
  *same row*. That carries two debts this design gets rid of: the row's log is
  the concatenation of both attempts, and the retried build is hazy about who
  ran it.

## What "stop" means

One signal. A build is "to be aborted" whenever it is no longer **ACTIVE and
owned by this worker** — status leaves ACTIVE, or ownership moves to someone
else:

```text
cancel_requested = (status != ACTIVE) || (worker_id != me)
```

That single condition unifies every way a worker can lose a build:

- a manual cancel (operator button / CLI) — status leaves ACTIVE;
- the reaper abandoning a too-long build — status leaves ACTIVE and the row is
  never reused, then a fresh build is queued;
- the reaper giving up on a package after its retry budget — status leaves
  ACTIVE, no replacement.

Because a build row is never reused, the *only* way a build id is ACTIVE is
under the worker that holds it: a worker never asks about a build that could be
ACTIVE-but-not-its-own. `status != ACTIVE` alone is the honest signal; the
`worker_id != me` clause stays as defence against a stale or buggy poll.

### One build row, one attempt

The reaper currently gets a silent/hung build requeued on the same row
(`requeue_or_fail`, `worker_jobs.rs:403-449`: `ACTIVE -> ENQUEUED`, bumbed
`attempt_count`, up to `MAX_ATTEMPTS`, then `-> FAILED`). Instead:

- the abandoned row is written **terminal FAILED** and left that way;
- a **fresh build row** for the same `(pkg_id, platform)` is inserted by
  `enqueue_build_if_missing` (`build_enqueue.rs:47`) — its partial unique index
  covers only pending states, so a terminal row never blocks it, and the
  `MAX(number)+1` race retry is already there.

This is cheaper than it looks, and it deletes several classes of problem:

- **Attribution per attempt.** A finished row's `worker_id` is the record of
  which machine ran it (`worker_complete.rs:170-180`). With reuse, the retry
  inherits the same row; with a fresh row, attempt 1 names worker A, retry names
  whoever actually ran it. No "is the new worker the same as the previous one"
  question ever needs asking.
- **Logs per attempt.** A requeued row's build log is attempt 1's output spliced
  onto attempt 2's. Fresh rows get fresh logs; the failed row ends with its own
  reason, the retry starts clean.
- **No shared state to race on.** Fresh build ids mean fresh
  `worker-staging/<id>` dirs and no row another worker can clobber. The
  aborted worker's report lands on a row nothing else will ever reuse.

The one thing that has to move is the retry budget: `attempt_count` lives on the
row and is meaningless once rows don't retry. Instead of a mutable counter, each
build row records **why it was created** — a `trigger` cause — and the budget is
*derived* from the build history: the number of **consecutive build rows for
this package that share the `timeout_retry` trigger and ended in abandonment**.
The reaper auto-queues a fresh build only while that run is below `MAX_ATTEMPTS`
(default 3).

Concrete rule, applied when the reaper abandons a build:

1. **Walk the package's build rows most-recent-first**, counting each trailing
   row whose trigger is `timeout_retry` **and whose `end_reason` is an
   abandonment** (`lease_expired` / `max_duration`) — i.e. rows this very
   mechanism queued after a futile run. The walk stops at the first row that
   fails that test: a `user` or `auto_update` trigger, a success, or a retry
   that ended some other way (`canceled`, a worker-reported failure). The just-
   failed row is part of the walk, so a retry that itself timed out continues
   the chain and an original `user` run does not.
2. **If the count is below `MAX_ATTEMPTS`**, insert the retry with trigger
   `timeout_retry`; otherwise stop — the budget is spent and the package stays
   failed.

Keying on *consecutive rows sharing the cause* gives the budget self-reset
semantics without any increment/reset logic to keep in sync:

- a **successful** build breaks the chain (the futile run is over);
- a **user-requested rebuild** (`user` trigger) breaks it — the operator
  deliberately intervened, so the budget restarts;
- a **version bump** (`auto_update` trigger) breaks it — the payload changed, so
  the old futile run no longer describes it;
- a **cancelled retry** breaks it too — `end_reason = canceled`, so the
  operator's intervention resets the budget without needing an extra manual
  rebuild.

A package that always exceeds the timeout stops after `MAX_ATTEMPTS` consecutive
futile retries, exactly as today. The history is the only state; nothing drifts
across a server restart, and a reaper pass needs at most one short read (`pkg_id`
+ `number DESC` limited to the budget).

A build's trigger is a small integer-coded enum on `builds.trigger`, defined in
`aurcache-common` (mirroring `BuildStates`, so the API and frontend share it),
backfilled to `user` for existing rows:

| Trigger | Who/what created the build |
|---|---|
| `user` | Operator add or rebuild (button, CLI) |
| `auto_update` | A version check found the package outdated |
| `timeout_retry` | Automatic retry of an abandoned build (this design) |

As a record of *why a build existed* the column is generally useful beyond the
budget: the build page can say "retry after timeout" vs "requested" vs
"auto-update" — a nicer answer than the concatenated-log confusion removal
above.

## Changes

### 1. Heartbeat response carries the abort list (server)

- `backend/aurcache-common/src/worker.rs`: new `HeartbeatResponse { cancel:
  Vec<i32> }` with stable field names, matching the "their JSON representation is
  a stable contract" note on that module.
- `backend/aurcache-db/src/helpers/worker_jobs.rs`: `heartbeat()` keeps its
  `renewed` / `dropped` fast path and adds a reported-id lookup. A reported id is
  `cancel_requested` iff it is missing, or its row is `status != ACTIVE`, or
  `worker_id != me`.
- `backend/aurcache-api/src/worker.rs::heartbeat` (line 784): return
  `Json<HeartbeatResponse>` instead of `()`.

A benign race: an id may be flagged the frame after the worker completed it,
because the id only leaves the worker's `active` set after the completion report
returns. Harmless — the flag is only read inside the build's kill-selection
loop, which has already exited by then.

### 2. Status GET implements the same rule (server fallback)

`backend/aurcache-api/src/worker.rs::job_status` (line 808) computes the one
rule above and returns it with no 403 for a build that left ACTIVE:

```rust
let cancel_requested = build.status != Some(worker_jobs::STATUS_ACTIVE)
    || build.worker_id != Some(auth.worker.id);
```

Only two hardening responses remain: `404` for a build that does not exist, and
`403` for a row still ACTIVE under a different worker — which, without row
reuse, a legitimate poll can never produce. Any worker only ever knows build ids
it was handed by a claim or a job descriptor, so there is nothing to withhold.

### 3. Abandon: fail the row, queue a fresh build (server)

This replaces `reap_expired_builds`' `requeue_or_fail` path
(`worker_jobs.rs:403-449`). For each build the reaper abandons (silent worker,
stale lease, or hung past the backstop) the whole resolution is **one
transaction**, with the terminal-status write as the decision point:

1. **CAS the row to terminal FAILED** — `status = ACTIVE AND worker_id = me AND
   lease_expires_at = <observed>` — setting `end_time` and a structured
   `end_reason`, keeping `worker_id` (the record of who ran the attempt) and
   clearing only the lease. The pin includes the **observed lease revision**, not
   just `status = ACTIVE`: a heartbeat renewal landing between the reaper's read
   and its write must make this a no-op (0 rows), exactly as `requeue_or_fail`
   pins the full observed row today (`worker_jobs.rs:426-437`). Without it, a
   healthy worker renewing its lease in that gap could have its live build
   terminally failed. If the CAS wins no row, **roll back and stop**: the build
   was renewed or completed in the gap, and nothing downstream runs.
2. **Mirror the package status to FAILED** and append the reason to the build
   log (the failed row tells the operator why, and the next attempt's log starts
   clean). Claiming set `packages.status = Building` (`worker_jobs.rs:314-320`),
   and only `worker_complete::finish_build` writes the terminal status back
   today (`worker_complete.rs:192-199`); abandonment must do the same or the
   package is stuck on "Building" forever.
3. **Decide the budget** (read the trailing consecutive `timeout_retry` run for
   this package within the same transaction) and, if it allows, **insert the
   fresh build** via `enqueue_build_if_missing` (`build_enqueue.rs:47`) —
`ENQUEUED`, trigger `timeout_retry`, and the version from the **current
    package metadata** (`pkg.upstream_version`), exactly as every other enqueue
    path does (`trigger_build_for_package`, enqueue.rs:250). The timeout may have
    coincided with the source moving on (an auto-update bumped the version while
    the old attempt hung), so "what should we build now" wins over "what the
    abandoned attempt captured"; retrying the superseded version would waste the
    budget on a build nothing would publish. AURCache deliberately retains a
    single source version per package — the current tree is the only source, and
    old attempts are not re-buildable — so this "current" is the only version
    that exists, not a cherry-picked one. The dedup that comes with this path
   is the right behaviour too: if a pending build already exists for the
   `(pkg_id, platform)` — a concurrent auto-update queued one — the insert is
   skipped (`inserted = false`) and the retry is not needed.
4. **Repoint the package row at the newest build**, mirroring exactly what the
   normal enqueue path does (`trigger_build_for_package`, enqueue.rs:265-269):
   if a retry was inserted, `packages.latest_build = <new id>` and
   `packages.status = ENQUEUED`; if the insert was skipped because a pending
   build already exists, the package already points at that build and nothing
   changes; if the budget was spent, `latest_build` still points at the failed
   attempt and `status` stays FAILED. A package must never read FAILED while a
   queued retry for it is pending.

Both outcomes — retry queued or budget spent — commit atomically; a failure
mid-transaction rolls everything back, so there is no "old build failed but no
retry" or "retry queued while the package still points at the failed attempt"
state. The fresh claim may land on any worker, and nothing about the old attempt
carries over: triggers, attribution, staging dirs and logs are all per-row.

**Termination reasons.** The terminal write captures a structured `end_reason`
(small integer-coded enum in `aurcache-common`, like `trigger`) rather than
leaving only prose in the log: `canceled` (operator `Action::Cancel`),
`lease_expired` (silent worker, filtered through the retry budget),
`max_duration` (the backstop fired on a still-heartbeating worker).
Worker-reported terminal outcomes (OOM, timeout, nonzero exit) keep their
existing `CompleteReport.reason` text in the log; mapping them into `end_reason`
is a straightforward extension if the UI wants one knob for all of them.

`Action::Cancel` is the same mechanism with the budget not consulted: one write
CAS-guarded on `status IN (ACTIVE, ENQUEUED, WAITING_FOR_DEPS)`
(`end_reason = canceled`), then the package mirror and log line — and no fresh
build. WAITING_FOR_DEPS is included because it is just as cancellable: nothing
has built yet, its dependents would block behind a failure either way, and
declining would strand a build a user stopped from reappearing until the queue
clears. If the CAS wins no row it all rolls back, so a cancel racing a
completion can never retroactively fail a build that finished.

### 4. Acknowledge a canceled report (server)

`complete_job` (`backend/aurcache-api/src/worker.rs:616`): before
`assert_owned_active`, a `CompleteReport { canceled: true, success: false }` is
accepted **only when the row is terminal FAILED and its retained `worker_id` is
the reporter** — 200, no ingest, never mutating the row. This is a real ownership
check, not "anyone may ack": a worker can only settle the record of an attempt
whose attribution names it, so an arbitrary authenticated worker that guesses an
in-flight build id is refused and cannot cause its staging directory to be
removed. Because a build row is never reused, the retained `worker_id` is always
the worker the abandoned attempt actually ran under, so the legitimate aborted
worker's ack always matches. `worker-staging/<id>` is removed only on such an
accepted ack. Everything else stays refused as today — notably a *success*
report for a row that is no longer owned (its upload must never be ingested).

### 5. Worker acts on the keepalive answer

- `backend/aurcache-worker-core/src/client.rs::heartbeat` (line 305): parse and
  return `HeartbeatResponse` instead of `Result<()>`.
- `backend/aurcache-worker-core/src/runner.rs::heartbeat_loop` (line 249): for
  each `cancel` id, set that build's `AtomicBool` in `active`. The existing 5 s
  tick inside `run_build` / `run_container` then aborts it. The self-abort
  watchdog (lost-server case) is untouched.

### 6. Kill the whole tree (worker, chroot executor)

- `backend/aurcache-worker/src/cgroup.rs`: `BuildCgroup::kill()` writing `"1"`
  to `cgroup.kill` (cgroup v2, recursive over descendants including
  `arch-nspawn`'s sub-cgroups; kernel ≥ 5.14, already implied by the
  `memory.peak` requirement of 5.19). The per-build cgroup exists precisely so
  the whole tree can be addressed; today it is only read for peak memory.
- `backend/aurcache-worker/src/job.rs` (line 538): on `canceled || hit_timeout`,
  kill via `cgroup.kill` primarily, with a real **process-group fallback** when
  no cgroup could be prepared — not a bare `child.start_kill()`. The child is
  spawned into its own process group (`Command::process_group(0)`), and the
  fallback sends SIGKILL to the negative group id (`libc::kill(-pid, SIGKILL)`),
  which takes `sudo` and `makechrootpkg` and the compilers with it instead of
  orphaning them.
- This also gives `WORKER_BUILD_TIMEOUT` real teeth.

Boundary to state plainly: the per-build cgroup is the only mechanism guaranteed
to cover the whole subtree even where `systemd-nspawn` hands the container to a
systemd-managed scope (which escapes the process group). On a systemd host
without a delegated cgroup, the group kill is best-effort and a scope-moved
container could survive. The deployments all prepare the cgroup hierarchy
(privileged container / `Delegate=yes`), so the fallback only ever sees simple
topologies.

The docker executor already stops the whole container
(`docker.kill_container`) — no change.

### 7. UI — Stop button

`frontend-rs/src/screens/build.rs`: a "Stop" button shown for non-terminal
builds (next to the worker, header at line ~340), wired to
`client.cancel_build(pkgbase, number)` (already in `aurcache-client`) then a
refresh, following the `RebuildButton` pattern (`package.rs:1368`). It stays
disabled-armed ("Stop…") while the request is in flight and re-enables on a
refusal, so a cancel that loses a terminal race does not wedge. The CLI
`aurcache build cancel` already exists.

## Consequences

- The worker now stops promptly: worst-case latency is heartbeat interval (15 s)
  + the 5 s tick + the kill; today "immediately" is a 30 s poll that never fires.
- Too-long builds are actually terminated, then retried as a *fresh* build up to
  the per-package budget. "Build #12 failed, #13 retried and succeeded" reads
  truthfully; the retry names the worker that ran it and has its own clean log.
- Cancelled builds still name the worker that ran them, and their package row
  leaves "Building".
- Canceled reports settle quietly; the server no longer logs a "refused
  completion" storm for a build it asked to stop, and cancellations no longer
  orphan a `worker-staging/` directory — the matched owner's ack cleans it and
  ids are never shared, so nothing else can clobber it.
- The per-row `builds.trigger` column supersedes `builds.attempt_count`: the
  retry budget is derived from the consecutive `timeout_retry` run rather than
  counted anywhere, and the same column answers "why did this build exist" on
  the build page. A structured `builds.end_reason` (`canceled` /
  `lease_expired` / `max_duration`) gives the same page a precise "why did it
  stop" instead of parsing log prose.

## Verification

- Unit: `heartbeat()` returns `cancel_requested` for reported-but-not-owned and
  missing ids, and leaves owned-ACTIVE builds alone.
- Unit: `job_status` applies the unified rule — terminal → cancel, other-owner
  ACTIVE → 403, missing → 404.
- Unit: the abandon transaction is atomic — a lease renewal in the read→write
  gap makes the CAS a no-op (pin includes the observed `lease_expires_at`) and
  nothing is written, no retry and no package change; a mid-transaction failure
  rolls back to a fully unchanged state.
- Unit: reaper abandon, when the CAS wins, writes terminal FAILED with the
  correct `end_reason` (keeping `worker_id`, mirroring the package status,
  appending the log line) and inserts a fresh build with trigger `timeout_retry`
  and the *current* package version while the budget allows — and repoints the
  package at the queued retry (`latest_build`/`ENQUEUED`); when the budget is
  spent, `latest_build` keeps the failed attempt and the package stays FAILED;
  when a pending build already exists, the insert is skipped and the package is
  left pointing at it. A success in the CAS gap leaves the row successful and
  queues nothing.
- Unit: the derived budget — a chain of N consecutive `timeout_retry` rows
  bounds auto-requeues at `MAX_ATTEMPTS`; a success, a `user` rebuild, or an
  `auto_update` bump in the trail breaks the chain and restores the budget.
- Unit: canceled-report accept is ownership-validated — 200, no row mutation, no
  ingest, staging removed — only when the terminal FAILED row's retained
  `worker_id` is the reporter; a non-owner (or an owner of a live build) is
  refused; success reports still refused everywhere appropriate.
- Unit: `end_reason` / `trigger` serialization round-trips and backfills to
  `user` / `None` for existing rows.
- Unit: runner heartbeat response ids map onto the per-build flags.
- Frontend: Stop button appears for running/pending builds and not for terminal
  ones; `scripts/test-frontend.sh` route assertion.
- E2E (stretch): cancel a build while it is building in `test-e2e.sh`.
- Integration (chroot executor): in the supported deployment shape (privileged
  container / `Delegate=yes`), start a real build, drive its
  `BuildCgroup::kill()`, and assert the subtree is gone — `build-<id>/cgroup.procs`
  empty, including any descendant cgroup `systemd-nspawn` created. Do the same
  for the `WORKER_BUILD_TIMEOUT` path, which shares the kill.

Full gate: `just lint`, `just test`, `just test-browser`.