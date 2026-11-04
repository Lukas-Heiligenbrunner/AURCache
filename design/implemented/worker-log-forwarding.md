# Design: Worker log forwarding

Status: **Implemented, first pass** · Last updated: 2026-09-22

Built: the two catalogue kinds, the `/api/worker/log` endpoint, the client and
protocol helper on the worker side, and seven call sites in `aurcache-worker`
instrumented as a first slice -- four in `job.rs`, three reached by threading
`report_to` through `Chroots::refresh`/`refresh_by_layer` into
`chroot::ensure_base_chroot`. Not done: the rest of the survey in
`design/implemented/structured-logs.md`'s worker section (`worker.claim_failed`,
`worker.heartbeat_failed`, `worker.lease_expired`, `worker.complete_report_*`)
-- see "Why the network-failure sites are not forwarded" below for why those
are deliberately left alone.

## The gap

`design/implemented/structured-logs.md` shipped structured, linkable server-side events,
but said at the top: "Workers log to their own journal; what the server learns
from them is recorded here." That was accurate at the time -- a worker's own
`tracing::warn!`/`error!` sites (in `aurcache-worker-core/src/runner.rs`,
`aurcache-worker/src/job.rs`, `chroot.rs`) stayed in that worker's own
container/journal. An operator debugging a fleet problem had no single place
to look; they had to know which worker to `docker logs`.

This design adds the missing leg: a worker-initiated channel that lets a
worker file a problem into the same `log`/`log_entity` store the rest of the
Logs page already reads from, so it shows up there and on the worker's own
page without an operator needing to go host by host.

## Shape: two kinds, not one, not many

The obvious first idea -- one `Event::WorkerLog { severity: Severity, .. }`
carrying whatever severity the worker reported -- conflicts with an existing,
enforced invariant from `structured-logs.md`: *severity is a property of the
kind, not of the row* ("two events of one kind cannot differ in severity").
Breaking that would have made this the first kind whose severity depends on
its payload, which no test and no other call site expects.

The other obvious idea -- one kind per worker call site, as
`structured-logs.md`'s own survey sketches (`worker.claim_failed`,
`build.spawn_retry`, `build.kill_fallback`, ...) -- gives the richest
sentences and filtering, but requires the wire to carry which of those kinds
a report is (a shared tag, or the server maintaining an ever-growing
catalogue in lockstep with worker releases). Workers and the server are
explicitly designed to run different versions (see the CA-rotation handling
in `aurcache-worker-core/src/enroll.rs`), so tying every new worker log site
to a server release is the wrong direction for this specific channel.

Landed instead: **two** kinds, split by severity, generic across call sites.

```rust
#[serde(rename = "worker.warning")]
WorkerWarning { worker: WorkerRef, build: Option<BuildRef>, message: String },
#[serde(rename = "worker.error")]
WorkerError { worker: WorkerRef, build: Option<BuildRef>, message: String },
```

Both kinds are added to `Event` in `aurcache-common/src/api/events.rs`
(`severity()`, `kind()`, `sentence()`, `KINDS`), so they get the existing
machinery for free: entity links, the picker, the frontend's generic renderer
(which decodes and matches `Event` -- no frontend-specific handling was
needed at all).

## Whether the worker and the DB need the same enum

No new enum crosses the wire. The worker sends a small, stable wire struct
(`aurcache_common::worker::WorkerLogReport { severity: Severity, build_id:
Option<i32>, message: String }`) -- reusing `Severity`, already shared
between client and server for the same "which kinds of things get filtered
together" reason `structured-logs.md` established. `message` is free text,
same as every `error: String` context value already in the `Event`
catalogue. The server maps `severity` onto exactly one of the two `Event`
kinds above; the DB's `kind` column, as always, just stores whichever tag
that produced. There is nothing for the worker and the DB to agree on beyond
`Severity`, which they already share.

## The build link is optional data, not a second kind

Most worker reports happen mid-build and are worth showing on that build's
(and its package's) log. Some are not about any one job -- fleet or chroot
maintenance runs independently of a build. This did not need two kinds
either: `build: Option<BuildRef>` on the same variant, `#[serde(default,
skip_serializing_if = "Option::is_none")]` so an absent build costs nothing
on the wire.

This works because `aurcache-activitylog::event::references()` already scans
every field of an event's serialized payload for `namespace:id` values and
turns each into an `(role, EntityRef)` row in `log_entity` -- an absent field
(skipped by `skip_serializing_if`) simply contributes no reference, so an
entry with `build: None` shows up under the worker's own page and the Logs
page's default view, and one with `build: Some(..)` additionally shows up
under that build and (per the existing `Build` special-case in
`references()`) its package. One field, one kind, both shapes.

## Ownership check: `assert_owned`, not `assert_owned_active`

`job_logs` (build output) uses `worker_complete::assert_owned_active`, which
also requires the build still be `ACTIVE`. A worker's problem report is a
comment on the build, not a claim on its state, and can legitimately arrive
just after the build's terminal state lands (cleanup, an upload still
finishing) -- rejecting it there would drop exactly the report an operator
most wants to see. Added `worker_complete::assert_owned`, the ownership half
without the active requirement, and used it for the new endpoint.

## Why the network-failure sites are not forwarded

Looking for call sites to instrument surfaced a real constraint:
`structured-logs.md`'s survey lists `worker.claim_failed`,
`worker.heartbeat_failed`, `worker.lease_expired`, and
`worker.complete_report_failed`/`_gave_up` as candidate worker-side kinds.
Every one of them fires *because the worker cannot reach the server* --
forwarding them over the same HTTP connection that just failed is
self-defeating, and is exactly the case the server already covers from its
own side (`Event::WorkerReaped`, emitted when a worker goes silent past its
lease). Those sites are deliberately left uninstrumented.

## What was instrumented (first slice)

Four sites in `aurcache-worker/src/job.rs`, chosen because each already has
`client` and `build_id` in scope, runs mid-build (so the connection that
would carry the report is, by construction, the one that just worked), and
is invisible today outside `docker logs`:

- persistent build directory unavailable (falls back to the chroot's own
  `/build`)
- promoting build output without a repository DB to validate against
- build spawn failed, retrying cold
- cgroup/process-group kill fallback (two sites, same shape)

All four call `aurcache_worker_core::protocol::report_warning(client,
Some(build_id), &msg)` right beside the existing `tracing::warn!`, best-effort
(errors logged at `debug!`, never propagated -- same rule as `protocol::log`
for build output: "a log must never be the reason the thing it is recording
got slower").

## Chroot maintenance: threading a client through, and why `build: None` still has no real caller

`ensure_base_chroot`'s refresh failure looked like the natural `build: None`
demonstration -- it is chroot-level, not build-level, machinery. Following its
callers up the stack (`Chroots::refresh` → `refresh_by_layer`) found the
opposite: every actual call to `refresh` in the running worker happens from
`run_job_inner` (`job.rs:222`), with a `client` and `build_id` already in
scope, because `Chroots` itself is constructed with no client at all --
`ChrootExecutor::new(cfg)` takes none, and the only other place a `Chroots` is
built, the crash-recovery `sweep()` in `main.rs::run`, runs *before*
`ensure_enrolled`, when no client exists yet to report to.

So `ensure_base_chroot`, `Chroots::refresh` and `refresh_by_layer` now take
`report_to: Option<(&WorkerClient, i32)>` -- an option one level up from the
`Event`'s own `build: Option<BuildRef>`, because here it is *whether there is
anyone to tell at all* that varies, not just whether a build applies. Two
`Chroots::refresh` callers exist: `job.rs` (`Some((client, build_id))`) and
`aurcache-worker`'s `build-once` dev CLI (`oneshot.rs`, `None` -- it builds a
local PKGBUILD with no server in the picture). This let three more sites ride
along for free once the plumbing was in place: the two `refresh_by_layer`
update failures (`could not update the chroot in place`, `could not update
the chroot`), alongside the original `ensure_base_chroot` one.

The upshot: in the worker's current shape, a `WorkerClient` only exists
*after* enrollment, and every long-lived piece of chroot/cache machinery that
runs after that point turns out to run from within a job. A genuine
`build: None` report -- something the worker notices independent of any
build, with a live connection to tell the server about it -- has no natural
call site yet. The `Option<BuildRef>` shape and its round trip stay covered by
a unit test (`one_of_each()`'s `WorkerError { build: None, .. }` fixture); a
real one waits on a piece of worker-lifetime (not job-lifetime) maintenance
that needs reporting, which does not exist today.

## Left for later

- Promoting a specific, high-value `worker.warning` site to its own typed
  kind (richer sentence, dedicated filter) once real usage shows which ones
  get looked at -- same "would anyone ever filter the two apart" test
  `structured-logs.md` already uses for consolidation.
- Batching multiple reports per request. Confirmed out of scope for this
  pass: one HTTP call per warning/error, matching `protocol::log`'s per-chunk
  shape.

## Verification

- `aurcache-common`'s existing exhaustive-match tests
  (`the_picker_lists_every_kind_once`, `every_kind_is_unique`,
  `every_event_round_trips_through_its_wire_form`,
  `every_reference_in_a_payload_is_in_its_sentence`) cover the two new
  variants via `one_of_each()`, including one fixture with `build: None` and
  one with `build: Some(..)`.
- Full backend workspace build and test pass green
  (`cargo build --workspace`, `cargo test --workspace`: 761 passed).
- `aurcache-frontend` (native `cargo check`) still compiles unchanged: the
  Logs page renders any `Event` generically from `kind`/`data`, so the two
  new kinds needed no frontend code.
