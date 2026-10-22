# Design: Structured operation logs

Status: **Implemented** · Last updated: 2026-09-21

Built: the catalogue, storage and its entity index, the `/log` endpoint, the
Logs page and the per-package and per-worker Activity cards reading from it,
always-valid links, the curated activity log folded in, and the server-side
sites ported -- builds (started, finished, failed, published, cancelled,
retried, deleted), worker registrations, configuration changes and timeouts,
settings, source edits, dependency changes, access, backups and the repository.
Left in the journal on purpose: startup and configuration messages, and
failures of the database itself. Workers log to their own journal; what the
server learns from them is recorded here. Once no deployment has rows left in
the `activity` table, a migration can drop it.

AURCache's logging today is rich in *sites* and poor in *shape*: every binary
logs through `tracing` with a plain text formatter (`aurcache/src/logger.rs`,
`aurcache-worker/src/main.rs`, `aurcache-worker-docker/src/main.rs` — all
`EnvFilter` at `info` by default), and nothing persists. The one structured
surface is the curated activity log, which already does the two things this
design wants — severity, and links to entities — but for a small set of
operator-facing rows.

This design introduces **structured operation logs**: every significant event is
a variant of one enum that serializes to a flat JSON payload, stored beside a
stable `kind`, a `severity` and a rendered `message`.

```rust
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", content = "data")]
pub enum Event {
    #[serde(rename = "deps.replaced")]
    DepsReplaced { dependent: PackageRef, old: PackageRef, new: PackageRef },
    // ...
}
```

```json
// Stored, and returned by the API as-is:
{
  "kind": "deps.replaced",
  "severity": "info",
  "ts": 1758030000,
  "message": "Replaced package foo with package bar as dependency of baz",
  "data": {"dependent": "pkg:baz", "old": "pkg:foo", "new": "pkg:bar"}
}
```

The payload key is a **role** (`old`, `new`, `dependent`); the value carries its
own type as a namespaced id (`pkg:foo`). Context values that name no entity are
ordinary scalars (`error`, `reason`, `attempt`). `message` is rendered at emit
time and stored. Every reference renders as a link; the page it leads to
decides what to do if its subject has since gone (see "Links").

There is **no primary subject**: every entity a row names is an equal citizen,
so "everything that relates to package foo" finds this row whether foo is the
removed dependency, the new one, or the dependent.

The dependency-replacement example is not hypothetical: it is the shape of
`PUT /package/<pkgbase>/dependency/<dependency>` (survey below), and it is
exactly the case where some entities are gone by the time anyone reads the row —
the old dependency is removed from the database by the same request that records
the replacement (see "Links, and the deleted-entity case").

The survey below is the argument that the material is there: AURCache already
has on the order of a hundred log sites, each with an implicit kind and a hand
formatted string that mixes entity names with prose. The design is mostly a
matter of naming what already exists and carrying the fields it already has in
its format string.

## What the activity log already settles

Two questions are already answered by the curated log, and the answers carry
over.

- **Severity is a property of the kind, not of the row.** `ActivityType`
  (`aurcache-db/src/activities.rs`) derives it by an exhaustive match, with a
  test that "ordinary news must never be filed as a failure". Two events of one
  kind cannot differ in severity — "publishing failed" and "package added" are
  not one event with a field.
- **Links are carried beside the text, never as markup inside it.**
  `ActivitySubject` is `Package | Worker | Build`, and the frontend finds the
  label in the prose and links it (`frontend-rs/src/screens/logs.rs`). The
  structured log keeps the separation and drops the prose-matching: a reference
  is a typed value in the payload, not a substring of a sentence.

One correction to the first revision of this document, which justified rendering
`message` at emit time as continuity with the activity log. The activity log
renders at **read** time — `serializer.format()` is called in `list_where`
(`activity_utils.rs:296`) — and the comment it cited
(`failure_activity.rs:9-12`) is about the captured `error` string inside a
payload, not about the sentence. Storing `message` is therefore a *departure*,
and a deliberate one, for a different reason: it is the fallback for a row whose
payload can no longer be parsed, which is the one failure the payload-as-record
model cannot otherwise survive.

## The survey: what the log sites look like

Every site below is a current `warn!`/`error!`/`info!` whose format string
already names the fields it wants. Listed as: current call site → what it
becomes. Line numbers are from the current tree.

### Version check and update scheduling (`aurcache-scheduler/src/update_version_check.rs`)

The densest cluster of per-package failures in the codebase — eleven sites,
every one already carrying `package.name`:

| Current site | Would-be kind (severity) | Fields |
|---|---|---|
| `Couldn't find {} in AUR response` (106) | `version_check.aur_missing` (warning) | `pkg` |
| `Failed to refresh git source for {}: {e}` (175) | `source.refresh_failed` (warning) | `pkg`, `error` |
| `Failed to refresh snapshot cache for {}: {e}` (165) | `source.refresh_failed` (warning) | `pkg`, `error` |
| `Failed to get sourceinfo for {}: {e}` (192) | `source.sourceinfo_failed` (warning) | `pkg`, `error` |
| `Failed to sync VCS sources for {}: {e}` (143, 213) | `vcs.sync_failed` (warning) | `pkg`, `error` |
| `Failed to store version check result for {name}: {e}` (272) | `version_check.store_failed` (warning) | `pkg`, `error` |
| `Cannot compare versions for {package}: upstream '{upstream}' vs built '{built}'` (259–262) | `version.compare_fallback` (warning) | `pkg`, `upstream_version`, `built_version` |
| `Failed to queue builds for newly outdated packages: {e}` (239) | `update.queue_failed` (error) | `error` |
| `Failed to perform aur version check: {e}` (26, + activity `VersionCheckFailed`) | `version_check.pass_failed` (error) | `error` |

The interesting field at 259–262 is the pair of *versions*; today they are
fused into one sentence only so a human can compare them. Structured, they
become filterable data and the catalog can render "upstream 3.2 vs built 3.1".

### Worker lifecycle and build reporting (`aurcache-worker-core/src/runner.rs`, `report.rs`)

`runner.rs` is the fleet's chronicle — claim, heartbeat, lease, completion:

| Current site | Would-be kind (severity) | Fields |
|---|---|---|
| `claim failed: {e}` (162) | `worker.claim_failed` (warning) | `error` |
| `heartbeat failed: {e}` (302) | `worker.heartbeat_failed` (warning) | `error` |
| `server asked to stop build #{id}; aborting` (297) | `build.cancel_requested` (info) | `build_id` |
| `server unreachable for {}s (> lease {}s); self-aborting {} build(s)` (309–314) | `worker.lease_expired` (error) | `build_ids`, `since_contact`, `lease_ttl` |
| `Build {build_id} failed: {reason}` (236–239) | `build.failed` (error) | `build_id`, `reason`, `exit_code` |
| `reporting completion for {build_id} failed: {e:#}` (249) | `worker.complete_report_failed` (warning) | `build_id`, `error` |
| `gave up reporting completion for {build_id}; server will requeue` (254) | `worker.complete_report_gave_up` (error) | `build_id` |

`report.rs` already *classifies* exit codes into kinds without naming them
(`classify_exit`, `report.rs:14-43`): exit 137 → OOM, 124 → timeout,
signal → killed, other → failed. Those become `build.oom`, `build.timeout`,
`build.killed_by_signal`, `build.failed` with `exit_code` and (for timeout)
`timeout_secs` as fields. The vocabulary exists; it is currently a string.

### Build execution (`aurcache-worker/src/job.rs`, `executor.rs`)

| Current site | Would-be kind (severity) | Fields |
|---|---|---|
| `build spawn failed ({e}); retrying cold` (575) | `build.spawn_retry` (warning) | `pkg`, `arch`, `error` |
| `cgroup.kill failed for build {build_id}: {e:#}; falling back...` (637–640) | `build.kill_fallback` (warning) | `build_id`, `error` |
| `process-group kill failed for build {build_id}: {e}` (648–651) | `build.kill_fallback` (warning) | `build_id`, `error` |
| `{} asked for a persistent build directory but one could not be prepared` (262–266) | `build.persistent_dir_unavailable` (warning) | `pkg`, `arch` |
| `promoting without a repository DB to validate against` (322) | `cache.promote_unverified` (warning) | `pkg`, `arch`, `error` |
| `package cache: promoted {promoted}, evicted {}` (335–338) | `cache.decided` (info) | `promoted`, `evicted` |

### Publishing (`aurcache-utils/src/publish.rs`)

| Current site | Would-be kind (severity) | Fields |
|---|---|---|
| `publishing build #{build_id} failed: {e:#}` (79, + activity `PublishFailed`) | `publish.failed` (error) | `build_id`, `pkg`, `error` |
| `Failed to trigger dependents of package {}: {e}` (72–75) | `dependents.trigger_failed` (error) | `pkg_id`, `error` |
| `could not mark build #{build_id} failed: {e}` (99) | `build.mark_failed` (error) | `build_id`, `error` |
| `could not write to the log of {pkgbase}/{number}: {e}` (110) | `build_log.append_failed` (warning) | `pkg`, `build_number` |

### Server-side ingest (`aurcache-api/src/worker.rs`)

| Current site | Would-be kind (severity) | Fields |
|---|---|---|
| `Failed to record peak memory for build {build_id}: {e}` (819) | `build.record_peak_memory_failed` (warning) | `build_id`, `error` |
| `Failed to record built VCS sources for build {build_id}: {e}` (836) | `build.record_vcs_failed` (warning) | `build_id`, `error` |
| `Failed to record failure reason for build {build_id}: {e}` (855) | `build.record_reason_failed` (warning) | `build_id`, `error` |
| `could not store worker {}'s configuration report: {e}` (890–893) | `worker.config_report_store_failed` (warning) | `worker`, `error` |
| `unreadable configuration report from a worker: {e}` (896) | `worker.config_report_unreadable` (warning) | `error` |

### Package edges and dependency replacement (`aurcache-api/src/package.rs`)

The one place the survey finds no `warn!`/`error!`, because there is nothing
today: `PUT /package/<pkgbase>/dependency/<dependency>` (`package.rs:1622-1712`)
repoints or drops a dependency edge and records no entry of any kind. The
events it should emit mention several packages each, and the old one is deleted
by the request's own `live_check` (`package.rs:1707`) when nothing depends on
it any more — so "linked, or not linked" has to be decided per entity, and for
`old_pkg` the honest answer at read time is usually "not linked" — and nobody
has to declare which of the three packages the row is *about*.

| Would-be kind (severity) | Fields | Notes |
|---|---|---|
| `deps.replaced` (info) | `dependent`, `old_pkg`, `new_pkg` | `new_pkg` may be added from the AUR by the same request (`ensure_replacement_exists` call, `package.rs:1676`) |
| `deps.dropped` (info) | `dependent`, `dropped` | the `replacement: None` arm (`package.rs:1634-1652`) |
| `deps.rejected` (warning) | `dependent`, `old_pkg`, `reason` | the refusals at `package.rs:1654-1686` — identical to current, self-dependency, nothing to replace, edge would be undone; today they exist only as response text a client shows once |

### Repository and cache maintenance (`aurcache-utils/src/repository.rs`, worker `cache.rs`/`chroot.rs`)

| Current site | Would-be kind (severity) | Fields |
|---|---|---|
| `repository update did not commit (attempt {attempt}): {e:#}` (`repository.rs:296`) | `repo.commit_retry` (warning) | `attempt`, `error` |
| `publishing a committed repository update panicked: {e}` (`repository.rs:312`) | `repo.publish_panicked` (error) | `error` |
| `could not move {} to {}: {e}` (`repository.rs:585`) | `repo.move_failed` (warning) | `from_path`, `to_path`, `error` |
| `could not remove {}: {e}` (`repository.rs:593`, `build_logger.rs:121,131`, `delete.rs:68`) | `repo.remove_failed` (warning) | `path`, `error` |
| `image pull reported: {e}` (`worker-docker/executor.rs:108`) | `chroot.image_pull_failed` (warning) | `error` |
| `chroot refresh returned non-zero:` (`chroot.rs:78`) | `chroot.refresh_failed` (warning) | `path`, `log` |
| `could not import pgp key {key}:\n{log}` (`chroot.rs:248`) | `gpg.import_failed` (warning) | `key`, `log` |
| `evicted cache entry {pkgbase}` (`cache.rs:654`) | `cache.evicted` (info) | `pkg` |

### Restore, bulk add, and misc server paths

| Current site | Would-be kind (severity) | Fields |
|---|---|---|
| `restore: could not read the source of {pkgbase}: {e}` (`restore.rs:374`) | `restore.source_failed` (warning) | `pkg`, `error` |
| `restore: could not resolve dependencies for {pkgbase}: {e}` (`restore.rs:400`) | `restore.deps_failed` (warning) | `pkg`, `error` |
| `bulk add could not batch-resolve pkgbases, continuing per package: {e}` (`bulk_add.rs:66`) | `bulk_add.batch_resolve_failed` (warning) | `error` |
| `could not record bulk add {job_id} progress: {e}` / `bulk add {job_id} ended abnormally` (`api/package.rs:206,214`) | `operation.bulk_add_*` (warning) | `job_id`, `error` |
| `Failed to issue server certificate, TLS disabled: {e}` (`api/init.rs:65`) | `server.tls_setup_failed` (error) | `error` |
| `\`SECRET_KEY\` env not set, generating random key.` (`api/init.rs:35`) | `server.ephemeral_secret` (warning) | — |
| `failed to build oauth redirect: {e}` (`api/auth.rs:133`) | `auth.oauth_redirect_failed` (error) | `error` |
| `Auto update skipped {package_name}: {e}` (`package/update.rs:112,133`) | `update.skipped` (warning) | `pkg`, `error` |
| `{} could not be parsed: {err:#}` (`snapshot.rs:811`) | `source.pkgbuild_unparseable` (warning) | `pkg`, `path`, `error` |

## The kinds, consolidated

Dotted `domain.verb.object` names, one dot-separated identity per row, grouped
by subsystem. Severity is derived from the kind by an exhaustive match (the
`ActivityType::severity` pattern), never carried by the caller:

- **version-check / update**: `version_check.pass_failed` (E), `version_check.store_failed` (W),
  `version_check.aur_missing` (W), `version.compare_fallback` (W),
  `source.refresh_failed` (W), `source.sourceinfo_failed` (W), `source.pkgbuild_unparseable` (W),
  `vcs.sync_failed` (W), `update.queue_failed` (E), `update.skipped` (W)
- **build (worker side)**: `build.failed` (E), `build.oom` (E), `build.timeout` (E),
  `build.killed_by_signal` (E), `build.cancelled` (I), `build.cancel_requested` (I),
  `build.spawn_retry` (W), `build.kill_fallback` (W), `build.persistent_dir_unavailable` (W)
- **build (server side)**: `publish.failed` (E), `build.mark_failed` (E),
  `build.record_peak_memory_failed` (W), `build.record_vcs_failed` (W),
  `build.record_reason_failed` (W), `build_log.append_failed` (W), `dependents.trigger_failed` (E)
- **worker / fleet**: `worker.claim_failed` (W), `worker.heartbeat_failed` (W),
  `worker.lease_expired` (E), `worker.complete_report_failed` (W),
  `worker.complete_report_gave_up` (E), `worker.config_report_store_failed` (W),
  `worker.config_report_unreadable` (W)
- **repository / cache / chroot**: `repo.commit_retry` (W), `repo.publish_panicked` (E),
  `repo.move_failed` (W), `repo.remove_failed` (W), `cache.promote_unverified` (W),
  `cache.decided` (I), `cache.evicted` (I), `chroot.image_pull_failed` (W),
  `chroot.refresh_failed` (W), `chroot.layer_flattened` (I), `gpg.import_failed` (W)
- **operations**: `restore.source_failed` (W), `restore.deps_failed` (W),
  `bulk_add.batch_resolve_failed` (W), `operation.bulk_add_progress_failed` (W),
  `operation.bulk_add_aborted` (W)
- **dependencies**: `deps.replaced` (I), `deps.dropped` (I), `deps.rejected` (W)
- **server / auth**: `server.tls_setup_failed` (E), `server.ephemeral_secret` (W),
  `auth.oauth_redirect_failed` (E)

That is on the order of 50 kinds from a first pass over ~100 sites — small
enough to be one variant each, large enough to be the app's failure vocabulary
(and due a consolidation pass, below).

The curated activity log's eleven written kinds are variants too, and the log
replaced it: `package.added`, `package.updated`, `package.deleted`,
`server.start`, `worker.enrolled`, `worker.approved`, `worker.revoked`,
`publish.failed`, `worker.reaped`, `version_check.pass_failed` and
`worker.setting_rejected`. Its rows are carried across at startup
(`aurcache_activitylog::legacy`) with their original time and actor, and leave
the old table as they land; a row that does not convert stays behind and is
reported. The reaper had recorded internal build row ids, which are translated
to `pkgbase #number` where the build still exists and dropped where it does not.

`package.updated` (and the `build.retried` that briefly joined it) later became
`build.queued`, one kind for every build request with its cause spelled out:
an *update* attempts a new version, a *rebuild* repeats one that worked, a
*retry* one that failed, a *dependency* build is queued on behalf of a
dependent, and a *first* build follows adding a package. The old `forced` flag
could not say which: the auto-updater forced every VCS package past the
version check, so its updates read as "forced". Migration
`m20260922_000000_build_queued_entries` rewrites the old rows, deciding a
person's forced request by the build before it. Around it, `version.detected`
records a new upstream version whether or not a build follows (auto-update may
be off), `build.unblocked` a waiting build freed by a dependency's publish or a
changed edge, and `build.published` carries the version it put in the
repository -- so one update reads as detected, queued, picked up, published.

## References and context values

The payload is one flat JSON object. The **key is the role** the value plays;
the **value carries its own type**.

### Entity references

Three newtypes, serialized as `namespace:id`, whose `Deserialize` rejects a
foreign prefix:

| Type | Wire | Route |
|---|---|---|
| `PackageRef` | `pkg:hello` | `/package/hello` |
| `WorkerRef` | `worker:builder-01` | `/worker/builder-01` |
| `BuildRef { pkgbase, number }` | `build:hello/7` | `/package/hello/build/7` |

Typing the *field* is what makes the encoding safe. A first revision put the
namespace in the key (`pkg.old`) so that storage could read the prefix
mechanically, and accepted that nothing stopped a role from holding the wrong
kind of id. With `old: PackageRef`, `"old": "build:baz/1"` is unwritable at the
call site and unreadable from the database, and the namespace is still there in
the value for anything that wants to scan for it.

It also removes the special cases the keyed form needed: a build is one
`BuildRef` rather than `build.base` + `build.number` (or a bare `build.id` the
reader has to map through the `builds` table), and a role naming several
entities is `Vec<BuildRef>` rather than a pluralised key.

### Context values

Anything that is not an entity: `error`, `reason`, `attempt`, `exit_code`,
`upstream_version`, `built_version`, `timeout_secs`, `since_contact`,
`lease_ttl`, `log`, `job_id`. Rendered as text or in a detail view; never
linked, never matched by the entity filter.

Durations are whole seconds, as everywhere else here (`units::format_duration`)
— not the floats the first revision used for `since_contact` and `lease_ttl`.

Display-only values (`platform`, `path`, `url`) are context, not references:
they carry no link, and nothing in the UI should try to make one.

## The emission surface

One enum, `aurcache_common::api::events::Event`, adjacently tagged: serde writes
`{"kind": .., "data": {..}}`, and the two halves are exactly the two columns a
row stores. No derive macro of our own, no registry, no `tracing` Layer.

```rust
impl Event {
    pub fn kind(&self) -> &'static str { /* exhaustive match */ }
    pub fn severity(&self) -> Severity { /* exhaustive match */ }
    /// Rendered now and stored, so the row still reads if its payload can no
    /// longer be parsed.
    pub fn message(&self) -> String { /* exhaustive match */ }
}

// The whole call site -- it replaces today's `warn!("…", …)` one for one:
log.emit(Event::DepsReplaced { dependent, old: current.name.into(), new: replacement.into() });
```

It lives in `aurcache-common` because the browser needs it: rendering an entry
from its payload means decoding it, and the frontend only builds for wasm
against the driver-free types.

Why an enum, after a revision that proposed one type per kind behind a trait:

- **Decoding is serde's job.** `decode(kind, data)` rebuilds the tagged form and
  lets serde dispatch; a per-type design needs a registry mapping kinds to
  decoders, kept in step by hand or by a linker-section crate.
- **Every consumer matches exhaustively.** `kind`, `severity`, `message` and the
  frontend's renderer are matches over the enum, so a new variant does not
  compile until each has said what it means. Severity cannot be forgotten.
- **The fields are the struct.** A struct variant cannot omit `new` or invent
  `nwe`, and serde refuses a payload missing a field on the way back.
- **Unknown fields are ignored, not refused.** `deny_unknown_fields` does not
  apply to enum variants, and for a log that is the right way round: a field
  added by a later version leaves a row an older reader can still render.
- **An unknown kind falls back to `message`.** A row from a newer server, or one
  whose payload no longer fits its variant, decodes to nothing and renders its
  stored sentence. That fallback is what the stored message is for.

The cost the trait design avoided -- one central list to edit for every new
event -- is accepted: it is the same list the exhaustive matches need anyway.

A kind that covers several cases keeps the difference as a field rather than as
a sub-kind column: `source.refresh_failed` carries `target: git | snapshot`, and
the sentence says which.

`emit` writes to the bounded queue the activity log already uses (`ActivityLog`,
`aurcache-activitylog/src/activity_utils.rs`) and calls `tracing::event!` beside
it with target `aurcache::event`, so the text logger keeps printing its line.
One source, two consumers. A full queue drops the entry rather than blocking
the caller: the log is a record, not a transaction, and nothing a caller could
do about a lost entry is worth making every call site handle it.

### Scope

A scope is an entity reference, so grouping needs no second vocabulary: a row
carries `scope: Option<EntityRef>`, and `build:hello/7` gathers both the events
*about* that build and those emitted *during* it.

It comes from a scoped handle — `let log = log.scoped(BuildRef { .. })` — rather
than a `tracing` span, because the log handle is already threaded through
`Services` and this keeps it explicit. If threading it down a deep call stack
proves to be the painful part, a `tokio::task_local` that `emit` consults when
the handle carries no scope is a drop-in upgrade. A `tracing` Layer is the only
option that needs the dynamic round trip, and is not planned.

The handle, storage and index are built; no production code emits through a
scoped handle yet, and no page shows a build's entries. That remainder is
`design/proposed/build-scoped-logs.md`.

## Links, and the deleted-entity case

1. **The record is pure.** An event says `old: PackageRef("foo")` and never says
   whether `foo` has a page. Baking a link (or its absence) in at emit time
   would let the row lie within seconds -- `live_check` can delete `foo` in the
   same request that records the replacement (`aurcache-api/src/package.rs`).
2. **Every reference is a link, and every link lands somewhere useful.** The
   page a link opens handles its subject being gone, rather than the log
   deciding in advance which links to draw:
   - a missing **package** redirects to the add page with its name searched --
     it may be something you want back;
   - a missing **build** redirects to its package's builds when the package is
     here, and to adding the package when it is not;
   - a missing **worker** redirects to the fleet list. A name several workers
     share still offers the choice between them.

   Each redirect is a history *replace*, so Back does not bounce off the
   missing page, and carries a notice explaining where you are. The notice
   lives in the shell, shown as a toast above any dialog (the add page is one),
   and expires once you move on from the page it explained.
3. **Nothing is resolved at read time.** An earlier revision had the API
   annotate each reference with a route or `null`, one batched existence query
   per namespace per page. It is gone: the answer was stale the moment it was
   read, it cost a query per page, and a `null` left the reader with plain text
   and nowhere to go. A 404 at the destination is the current answer, and the
   redirect makes it a useful one.

`message` keeps the names either way, so the sentence reads the same whatever
has happened to what it names.

## Storage

One log store, not two. The survey is dominated by failure paths, so the volume
is hundreds of rows a day rather than millions, which makes a second table with
its own retention unjustified. The curated activity events become `Event`
variants in the same store, and the existing retention sweep and pager apply
unchanged.

```
log(id, kind, severity, message, ts, user, scope, data TEXT)
log_entity(log_id -> log(id) ON DELETE CASCADE, role, ns, id)
  PRIMARY KEY (log_id, role, ns, id);  INDEX (ns, id)
```

At insert, walk the serialized payload (and `scope`) for every `namespace:id`
and write the rows in the same transaction, keeping the payload key as `role`.

A build is also filed under its package, in the same role. A build is part of
its package's story: "everything about yay" that left out "publishing yay #7
failed" -- or what was recorded during that build -- would answer a narrower
question than the one asked.

Carrying `role` is what keeps **every** query off JSON: a role-scoped filter is
a column comparison rather than `data->>'dependent'`, which sea-query only
exposes as a Postgres operator (`PgBinOper::CastJsonField`). `data` is then
written and read whole, and nothing queries into it.

**Why a side table rather than a `refs jsonb` column with a GIN index.** The
jsonb form indexes better on Postgres — `refs @> '["pkg:foo"]'` against
`gin (refs jsonb_path_ops)` is the best-indexed shape Postgres offers — but the
filter would then be a *different query per backend*. Every test in this repo
connects to `sqlite::memory:`, and `docker-compose.e2e.yaml` sets no `DB_TYPE`,
so nothing is exercised against Postgres anywhere: the branch the tests cover
would be the one that never runs in production. A side table is one query on
both backends, expressible in sea-orm without `PgBinOper` or a raw-SQL index, so
the SQLite tests exercise the SQL that actually ships — and backend-agnostic
queries are the standing preference here, with anything backend-specific to be
raised before it is written.

The usual objection to a derived index — that it drifts from what it indexes —
is weak here because the log is append-only: rows are never mutated, and deletes
come only from the retention sweep, which one `ON DELETE CASCADE` covers. What
remains is a bug in the extraction function, and that risk is identical for a
derived `refs` column.

Revisit the jsonb form if a Postgres test lane is ever added and the side
table's write cost starts to matter.

### The queries

```sql
-- everything about one entity, whatever role it played
WHERE id IN (SELECT log_id FROM log_entity WHERE ns='pkg' AND id='foo')

-- mentions both
... GROUP BY log_id HAVING COUNT(*) = 2

-- one role specifically
WHERE id IN (SELECT log_id FROM log_entity
              WHERE role='dependent' AND ns='pkg' AND id='foo')
```

All three are ordinary SQL that sea-orm builds identically for both backends —
no JSON operators, no `PgBinOper`, no raw SQL, and so no branch that only one
backend's tests would cover.

None of them names a kind, so they run against rows of kinds that did not exist
when the query was written. A *family* of events ("worker events") is
`kind LIKE 'worker.%'` on the log table itself — the UI holds the catalogue and
crafts that; it is not a storage concern.

`?build=hello/7` returns both the events about that build and those emitted
under its scope, because `scope` is extracted into `log_entity` alongside the
payload's references.

## Frontend rendering

The Logs screen renders each row from its payload:

1. `kind` + `data` decode to an `Event`; a known variant renders its sentence
   with each reference as a link to its route (`/package/…`,
   `/package/…/build/…`, `/worker/…`).
2. `message` is the fallback when the kind is unknown (a newer server) or the
   payload no longer fits its variant.
3. The entity filter is the server-side query above, carried in the URL as
   `/logs?e=pkg:hello`. A package's and a worker's page link to their own; the
   Logs page shows the active filter and lets it go, but does not
   re-implement it.

Localization becomes a change to the frontend's renderer alone; `message` never
changes, so non-UI consumers are unaffected.

## Consolidation to do while porting

The catalogue above has several kinds that are one-per-call-site:
`build.record_peak_memory_failed`, `build.record_vcs_failed` and
`build.record_reason_failed` are three kinds for "a database write about a build
failed". Merge where an operator would never filter on the difference, keeping
the distinction as a context value. The test is whether anyone would ever want
the two apart in a filter.

Also: several events are user-initiated (`deps.replaced` comes from a `PUT`),
so the record needs the actor the activity log already carries as `user`.

## Non-goals

- Parsing or linking entities out of `message` text. A reference is a typed
  value in the payload or it is not a reference. (The `split_on` whole-word
  linker in `logs.rs` stays only for activity prose written before this design,
  where the prose is all there is.)
- A primary subject, or any ranking among the entities a row names.
- Resolving entity existence in the browser.
- Structured *build logs*. Per-line instrumentation of a makepkg build is out of
  scope; what gets structured here is the events *about* builds, not the bytes
  of their output. The build log stays a bounded byte stream
  (`design/implemented/build-log-capping.md`).
- OTel/metrics export, full-text search, or log shipping. The shape is chosen to
  be export-friendly, but export is a later step.

## Verification

- **Round trip**: every `Event` variant serializes and decodes back unchanged;
  a payload missing a field is refused, and one with an extra field is not.
- **Reference typing**: a `PackageRef` field refuses `worker:x` and
  `build:x/1`; `BuildRef` round-trips `build:hello/7`, including a pkgbase
  containing `-` and `+`.
- **Fallback**: a row whose payload no longer matches its type renders from the
  stored `message` and is not dropped — the failure the stored message exists
  for, and the one the current activity log gets wrong by skipping the row while
  still counting it.
- **Entity filter**: `?pkg=foo` finds a row where `foo` is the old dependency,
  the new one, and the dependent; the test asserts the query names no kind.
- **Extraction**: `log_entity` rows regenerated from a payload match what was
  written, for every event type — the one place a side table can drift.
- **Links**: a link to a missing package, build or worker lands on the add
  page, the package's builds, or the fleet, with a notice saying why
  (`frontend-rs/tests/browser.rs`).
- **Scope**: events emitted under `log.scoped(build)` carry it, and
  `?build=hello/7` returns both those and the events naming that build.
- **Severity**: stored from the variant's exhaustive match, so a row's severity
  cannot disagree with its kind at write time.
- `scripts/test-frontend.sh` for the Logs screen, which already covers the page,
  both filters and entity links.
