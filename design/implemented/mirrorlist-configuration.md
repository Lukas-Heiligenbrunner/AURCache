# Mirrorlists and per-architecture dependency resolution

Plan for making the pacman mirrorlist independently configurable on the server
and on each worker, delivering it at registration rather than per build, and
resolving official-repo dependencies per architecture.

These began as two topics and are one: fetching official repo databases for a
non-x86_64 architecture is impossible without that architecture's mirrorlist,
because Arch and Arch Linux ARM do not share a URL layout. See
"Per-arch dependency resolution" below.

Status: the mirrorlist half is **implemented**. The per-arch dependency
resolution half (second part of this document) is **not**, and is deliberately
optional -- see the judgement recorded there.

---

## What exists today

One directory (`AURCACHE_MIRRORLIST_DIR`, default `./repo`) holds files named
`mirrorlist.<arch>`. Three things write it and two things read it, and the two
readers want different things.

### Writers

| Source | Effect |
| --- | --- |
| `MIRRORLIST_SERVERS_X86_64` (semicolon-separated) | `startup.rs:139` writes `mirrorlist.x86_64` |
| a bare `mirrorlist` file mounted in | `adopt_shared_mirrorlist()` adopts it into the native arch slot |
| `MIRROR_RANK_SCHEDULE` (default weekly) | `aurcache-scheduler/src/mirror_ranking.rs` re-ranks and rewrites `mirrorlist.x86_64` |

When none of these has produced a file, `startup.rs` ranks official Arch
mirrors itself via `pacman_mirrors::get_status`. **The server is therefore
never without a usable x86_64 mirrorlist, configured or not.**

### Reader A — the server's own index fetch

`aurcache-deps/src/repo.rs` reads `mirrorlist_path("x86_64")` (override:
`OFFICIAL_MIRRORLIST_PATH`, a *path*) and downloads `core.db`, `extra.db`,
`multilib.db`, cached one hour. It exists to answer
`cached_official_dependency_exists(dep)` — "is this already in the official
repos, so we needn't build it from the AUR?".

Index only, never packages. **x86_64 is hardcoded**: `official_repo_db_url`
substitutes `$arch` → `x86_64` literally, so pointing this at an ALARM mirror
would silently fetch x86_64 databases.

### Reader B — the workers

`aurcache-api/src/worker.rs:390` calls `mirrorlist_for(&arch, ...)` **per
claimed job** and puts the content in the job config; `chroot.rs:226` writes it
into the build chroot. A missing or empty file yields `None`, and the worker
falls back to its image's own `/etc/pacman.d/mirrorlist`.

Since only `x86_64` is ever written, foreign-arch workers already fall back to
their own mirrors. That fallback path works and is load-bearing.

---

## Against the requirements

| Requirement | Today |
| --- | --- |
| Optional on both sides | **already true** — server self-ranks, worker falls back to its image |
| Server-only config used everywhere | **already true** — the server's list ships to every worker |
| Workers can override | **missing** — no worker-side setting, and the shipped list wins unconditionally |
| Sent at registration, not per build | **missing** — currently per job |
| Per-arch mirrorlists | **half** — the `mirrorlist.<arch>` layout exists; only x86_64 is ever populated |

The first two need no work beyond confirming them in docs.

### Do workers build only for their own arch?

**No.** A worker registers `native_arches` *and* `emulated_arches`
(`aurcache-common/src/api/worker.rs:24`), so one worker may be routed jobs for
several architectures. Registration-time delivery must therefore send a **map
of arch → mirrorlist**, not a single string.

The map should be filtered server-side to the arches that worker declared:
sending an armv7h list to an x86_64-only worker is noise, and the server
already knows the declared set from the registration payload.

---

## The `repo_template` precedent, and why it does not apply

`RegisterStatus.repo_template` (`aurcache-common/src/worker.rs:69`) is the
existing example of the server handing a worker a piece of configuration, and
its doc comment gives the rule:

> Sent at registration rather than per job because it describes the deployment,
> not the build.

It is tempting to file the mirrorlist under the same rule. It does not belong
there: `repo_template` is **static** for a deployment, whereas the mirrorlist
is rewritten on a schedule by `MIRROR_RANK_SCHEDULE`. The rule is about values
that do not change, and applying it to one that does creates a staleness
problem rather than solving anything. See section 3 below.

## Proposed design

### 1. Generalise the server's per-arch config

Accept `MIRRORLIST_SERVERS_<ARCH>` for any architecture —
`MIRRORLIST_SERVERS_AARCH64`, `MIRRORLIST_SERVERS_ARMV7H` — each writing
`mirrorlist.<arch>`. `MIRRORLIST_SERVERS_X86_64` keeps working unchanged; it
simply stops being special-cased.

Mirror ranking stays x86_64-only: `pacman_mirrors` ranks Arch mirrors, and
there is no equivalent for Arch Linux ARM. Other arches are configured or
absent.

### 2. Separate the index fetch from what workers get

Add `OFFICIAL_MIRRORLIST_SERVERS`, a server list in the same shape as
`MIRRORLIST_SERVERS_X86_64`, used only for the official-repo index fetch. It
defaults to the x86_64 list, so existing deployments are unaffected.

This is what lets a server keep a fast local mirror for its own index while
workers elsewhere use theirs. `OFFICIAL_MIRRORLIST_PATH` stays as the
lower-level path override.

### 3. Deliver on job claim, revalidated by checksum

The mirrorlist is only ever used *during a build*, so it is delivered when a
job is claimed. To keep that from meaning "the same bytes with every job", the
claim revalidates a checksum:

* `ClaimRequest` carries what the worker already holds. Its body is currently
  accepted and discarded (`worker.rs:344`, bound as `_claim`), so this needs no
  new endpoint and no new round trip.
* The server compares against the mirrorlist for the job's architecture and
  includes the content only when it differs.

```rust
/// What the worker wants for this build's mirrorlist.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "mode")]
pub enum MirrorlistPreference {
    /// The worker has its own; send nothing.
    Local,
    /// The worker takes the server's, and holds these checksums by arch.
    Server { checksums: BTreeMap<String, String> },
}
```

**Keyed by architecture, and this was a correction made while implementing.**
The claim is sent *before* the server picks a job, so the worker does not yet
know which of the architectures it builds the answer will be for. A single
checksum cannot express that. It is a map of checksums, not of contents, so it
stays small.

**The server computes the checksum; the worker only echoes it back.** Nothing on
the worker side digests anything, so the two ends cannot disagree about the
algorithm, and changing it later costs one resend per worker rather than a
coordinated upgrade.

`JobDescriptor` correspondingly carries a three-valued `MirrorlistOffer`:
`Send` (new content and its checksum), `Keep` (the copy the worker advertised
is current) or `Nothing` (none configured). It was first encoded as three
loose fields, so that a worker from before the change could still read it;
workers and servers ship together, so it is now the enum itself, and the
worker resolves it into the content the build uses before an executor sees
the job.

Distinguishing "keep what you have" from "the server has none" matters: the
second has to make the worker *drop* its cached list, or a mirrorlist removed on
the server would live on in every worker.

The worker keeps a small local `arch -> (checksum, content)` cache, so
alternating between architectures does not refetch.

#### Why not at registration, like `repo_template`

An earlier draft of this document proposed registration-time delivery by
analogy with `repo_template`, and then had to solve a staleness problem that
the analogy itself created. The analogy does not hold: `repo_template` is
**static** for a deployment, while the mirrorlist is rewritten on a schedule by
`MIRROR_RANK_SCHEDULE`. "Describes the deployment, not the build" is the right
rule for a value that does not change; it is not a reason to push a changing
value down a path that cannot refresh it.

Claim-time revalidation is strictly better here:

* **No staleness window.** Every build uses the current list. Registration-time
  delivery would hold a worker's list until it restarted, and would need a
  second mechanism to fix that.
* **No per-arch map of mirrorlist *contents*.** A job has exactly one
  architecture, so only that arch's list is ever sent. The claim still carries
  one checksum per arch the worker holds, because it precedes job selection —
  but checksums are small, where the "arch-keyed map of contents filtered to
  the worker's declared arches" an earlier draft proposed was not.
* **Nothing sent when nothing changed**, and nothing sent at all to a worker
  that overrides locally.

Registration keeps carrying `repo_template` and gains nothing.

### 4. Worker-side override

Add `WORKER_MIRRORLIST_SERVERS` (same list shape) and
`WORKER_MIRRORLIST_FILE` (a path, for a mounted file). Precedence per build:

1. the worker's own setting, if set
2. the server-supplied list for that arch, if present
3. the worker image's `/etc/pacman.d/mirrorlist` — i.e. write nothing

Only step 2 is new behaviour; 1 is the new knob and 3 is today's fallback.

### 5. Make the per-job send conditional

`mirrorlist_for` (`worker.rs:390`) stays, but its result is checksummed and
sent only when the worker's claim says it holds something else. The worker
resolves by the precedence in (4) rather than writing whatever arrives.

---

## Files touched

| File | Change |
| --- | --- |
| `aurcache-common/src/worker.rs` | `MirrorlistPreference` on `ClaimRequest`; three-valued `mirrorlist` on `JobDescriptor` |
| `aurcache-api/src/worker.rs` | use the claim body: checksum the arch's mirrorlist, send content only when it differs |
| `aurcache-utils/src/job_config.rs` | keep `mirrorlist_for`; add the checksum helper |
| `aurcache/src/startup.rs` | generalise to `MIRRORLIST_SERVERS_<ARCH>` |
| `aurcache-deps/src/repo.rs` | `OFFICIAL_MIRRORLIST_SERVERS` |
| `aurcache-worker-core/src/config.rs` | `WORKER_MIRRORLIST_SERVERS`, `WORKER_MIRRORLIST_FILE` |
| `aurcache-worker/src/job.rs`, `chroot.rs` | send the preference, cache `arch -> (checksum, content)`, resolve by precedence |
| `docs/` | document all of it; none of it is currently documented |

---

## Open questions

1. Resolved: claim-time checksum revalidation. No staleness window, no
   per-arch map, no new endpoint.
2. `WORKER_MIRRORLIST_SERVERS` is per-worker, not per-arch. A worker building
   two arches with its own mirrors for both would need
   `WORKER_MIRRORLIST_SERVERS_<ARCH>`. Worth it, or is the single-arch case
   enough until asked for?
3. Should a worker override *replace* the server's list, or should the server
   be able to mark its list mandatory? Replace is simpler and matches "workers
   can override"; a mandatory mode is a policy knob nobody has asked for.


---

# Per-arch dependency resolution

## The bug

`AurClient::resolve_dependencies` (`aurcache-deps/src/client.rs:218`) decides
whether each dependency is satisfied by an official repository:

```rust
if self.local_repo_dependency_exists(dep_name)?
    || self.official_dependency_exists(dep_name).await
{
    resolutions.insert(dep_name.to_string(), DependencyResolution::Official);
}
```

`official_dependency_exists` takes **no architecture**. It consults
`core`/`extra`/`multilib` for `x86_64` only, because `official_repo_db_url`
hardcodes `$arch` → `x86_64`. The verdict is then applied to the package for
every platform it is configured to build for.

`DependencyResolution::Official` means "do not build this; pacman will provide
it". So a dependency that exists in Arch but not in Arch Linux ARM is recorded
as satisfied, no AUR build is scheduled, and the aarch64 or armv7h build fails
at dependency install with `target not found`.

### What is already arch-aware

Dependency *extraction* is: `resolve_srcinfo_to_spec` is passed
`architectures_for_platforms(&context.platforms_str)` (`add.rs:407`, `:441`),
so `.SRCINFO`'s per-arch `depends_<arch>` arrays are honoured. It takes the
**union** across the package's configured platforms, though, so a package built
for both `x86_64` and `aarch64` pulls `depends_x86_64` into one shared graph.

So extraction knows about architectures and resolution does not.

### Ranked by realism

**All of these are rare in absolute terms**, and the ranking below is only
relative. Arch Linux ARM rebuilds essentially all of Arch's `core`/`extra`, so
the gap is thin, and a package has to be configured for ARM *and* land in that
thin gap. It is also not a silent failure: the build stops at pacman rather
than producing a wrong package.

1. **A package in Arch `extra` but absent from or lagging in ALARM.** Recorded
   Official, never built, ARM build fails. The most likely of the three.
2. **`multilib`.** It is in `OFFICIAL_REPO_NAMES` and is x86_64-only. Partly
   mitigated because `lib32-*` deps are normally declared under
   `depends_x86_64` — but the union extraction above defeats that mitigation
   for a package configured for several arches.
3. **Present in ALARM, absent from Arch.** Resolves to AUR and is built
   needlessly. Wasteful, not broken.

## Why this needs the mirrorlist work

The URL layouts differ:

```
Arch:            Server = http://mirror/$repo/os/$arch    -> extra/os/x86_64/extra.db
Arch Linux ARM:  Server = http://mirror/$arch/$repo       -> armv7h/extra/extra.db
```

`official_repo_db_url` substitutes `$repo` and `$arch` into whatever the
mirrorlist says, so it produces a correct URL for either layout — **but only if
it is given that architecture's mirrorlist.** Substituting `aarch64` into an
Arch mirror template yields a path no mirror serves.

Per-arch official databases therefore require per-arch mirrorlists. One change.

`OFFICIAL_REPO_NAMES` also becomes per-arch: `core`/`extra`/`multilib` for
x86_64, `core`/`extra`/`alarm` for the ARM architectures (ALARM has no
`multilib` and adds `alarm`).

## Invariant: single-arch installs are untouched

The architecture set used for resolution is **the package's own configured
platforms** (`context.platforms_str`) — never the instance's supported set,
never the arches its workers advertise.

A package configured for `x86_64` alone is therefore evaluated against
`{x86_64}`, the "official for every configured arch" predicate reduces to
today's single check, and behaviour is byte-identical: no AUR build is
introduced because some *other* architecture lacks the package. An install that
only ever requests x86_64 packages sees no change at all, and the same holds
for any future architecture (risc-v is not a `Platform` today; `Platform` is
`X86_64 | Aarch64 | Armv7h`).

Reading that set from the wrong place is the obvious way to regress this, so it
wants a test asserting an x86_64-only package resolves identically before and
after.

## Storage

`dependencies` (`aurcache-db/src/dependencies.rs`) is
`dependent_id`/`dependee_id`/`version_constraint` — **no platform column**. The
graph is global, so there is currently nowhere to record "official on x86_64,
must be built on aarch64".

Four ways out, counting doing nothing:

0. **Leave it.** The failure is loud — pacman stops the ARM build — rather than
   silently producing a wrong package, and the situation is rare. Given the
   cost of (1) below, this is a serious candidate, not a strawman.
1. **Conservative resolution, no schema change.** A dependency counts as
   Official only when it is present in the official repos of *every*
   architecture the package is configured for; otherwise it is resolved
   normally (AUR, or omitted). Errs toward building, which is the safe
   direction: `local_repo_dependency_exists` is consulted first, so a
   locally-built copy simply wins over the official one.
2. **Per-arch dependency rows.** Add `platform` to `dependencies` (NULL = all).
   Exact, and a migration plus changes everywhere the graph is read.
3. **Resolve per build.** Correct by construction, but moves resolution out of
   add-time into the build path — a much larger change.

**Recommendation: (1), and only as a follow-on.** It carries no regression
risk — a dependency that resolves to neither an official repo nor the AUR is
simply omitted from the map (`client.rs:252`), which is exactly today's
behaviour for a wrong `Official` verdict — and it fixes every case where the
AUR does have the package.

**(2) is rejected**, not merely deferred: a migration plus changes to every
reader of the dependency graph is disproportionate to a rare failure that stops
the build loudly instead of shipping something wrong. (3) likewise.

**The cost of (1)**, and the reason (0) stays on the table: for a package
configured for several arches, where a dep is in Arch but not ALARM *and* an
AUR package of that name exists, (1) builds it — and because AURCache's own
`[repo]` takes precedence inside the build chroot, the **x86_64** build then
resolves that dependency to our AUR-built copy instead of the official package.
A silent substitution on an architecture that was working, traded for a loud
failure on one that was not. Single-arch packages are unaffected either way
(see the invariant above).

This whole section is **not a prerequisite** for the mirrorlist work and should
not gate it. The dependency fix only becomes cheap *because* per-arch
mirrorlists exist — at that point it is one function signature, and worth doing
only if it stays that cheap.

## Which arches to cache

Per the requirement: any architecture with at least one build. In practice the
distinct set of `packages.platforms` across the instance. Fetching is cheap
(three `.db` files per arch, hourly TTL) so the union of configured platforms
is a fine approximation; there is no need to key it off build history.

## Additional files touched

| File | Change |
| --- | --- |
| `aurcache-deps/src/repo.rs` | per-arch cache dir, per-arch `OFFICIAL_REPO_NAMES`, drop the hardcoded `x86_64` |
| `aurcache-deps/src/client.rs` | `official_dependency_exists(dep, arches)`; conservative rule |
| `aurcache-utils/src/package/add.rs` | pass the configured arches into resolution |

## Additional open question

4. Resolved: build from the AUR for all arches (option 1), as a follow-on once
   per-arch mirrorlists exist, and only if it stays to roughly one function
   signature. Per-arch dependency rows are rejected as disproportionate.
