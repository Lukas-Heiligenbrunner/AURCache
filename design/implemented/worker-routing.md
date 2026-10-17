# Design: Worker Affinity and Priority Routing

Status: **Implemented** · Last updated: 2026-09-22

Extends [`remote-workers.md`](remote-workers.md). That design routes jobs by
architecture only: `claim_job` prefers native arches and reserves a foreign arch
for a worker that can build it natively. This document adds two orthogonal
routing dimensions on top of it.

## Motivation

Two independent problems, both currently unsolvable:

1. **Some packages can only be built on some machines.** `unreal-engine` needs a
   GitHub account linked to the Epic Games organisation and an SSH key that can
   `git clone git@github.com:EpicGames/UnrealEngine`. That credential lives on
   one specific machine. If any other worker claims the job, the build fails —
   it does not fall back, it just burns an attempt and a retry budget.
2. **Not all workers are equally good.** The intended topology is a slow
   fallback worker in the same `docker-compose` as the backend (always up), plus
   fast workers elsewhere on the LAN that come and go. Jobs should land on a
   fast worker whenever one has capacity, and only spill onto the fallback when
   the fast ones are saturated or absent.

These want different mechanisms and should not be conflated:

| Dimension | Meaning | Enforcement |
|---|---|---|
| **Affinity** | *Can* this worker build this package at all (credentials, licensed toolchain, disk)? | **Hard** — a non-matching worker is never handed the job. |
| **Priority** | *Should* this worker get work before another one? | **Soft** — a hold-back that expires, never a permanent block. |

A hard rule that can starve is only acceptable when the alternative is a
guaranteed failure (affinity). Speed preferences must always degrade to "someone
builds it" (priority).

---

## Part 1 — Package affinity

### Model

A worker declares a list of packages it is specially provisioned for:

```sh
WORKER_PACKAGES=unreal-engine,other-restricted-pkg
```

The list is sent at registration and stored on the `workers` row. The server
derives the routing rule from the union of all *approved* workers' lists:

> If a package matches the affinity list of **at least one approved worker**,
> then **only** workers whose list matches it may claim builds for it. If no
> approved worker claims affinity for a package, every worker may build it.

This is exactly the shape of the existing `arches_with_native_worker`
reservation, so the two compose without special cases: a foreign-arch build of
an affine package goes to a worker that is both native for the arch and affine
for the package.

Matching is against `packages.name` (the pkgbase — AURCache's unit of build),
as exact, case-sensitive names. **No wildcards.** Beyond keeping the matching
trivial, an over-broad pattern is a genuine hazard here: it would silently
reserve packages nobody intended to restrict, and every one of them would stall
the moment that worker went away. An explicit list cannot fail that way, and it
keeps `WORKER_PACKAGES` readable as a statement of what the machine is
provisioned for. Since AURCache's unit is the pkgbase, a split package needs one
entry regardless of how many `pkgname`s it emits.

Wildcards can be added later if a real need appears, and doing so is cheap and
backward compatible: an exact name is just a pattern containing no `*`, so every
existing `WORKER_PACKAGES` value keeps its current meaning. The only code that
changes is the affinity index build (Implementation, step 3), which reverts from
a direct inversion of the worker rows to evaluating patterns against the distinct
candidate package names. The index type, the predicates, and every call site are
unaffected.

### Why worker-declared rather than package-tagged

The capability is a property of the machine (it holds the key), so the machine
is the natural place to declare it — one config file, no risk of adding
`unreal-engine` in the UI and forgetting to tag it. The trade-off is that
provisioning a *new* restricted package means editing worker config and
restarting the worker rather than clicking in the UI.

If a third dimension ever appears (a licensed toolchain, a machine with 500 GB
of scratch), the generalisation is the Jenkins/GitLab-runner model: packages
carry required *labels*, workers advertise provided labels. The scheme here is
the special case where the label is the package name, so that migration is
additive — a `packages.required_labels` column plus one extra term in the
predicate. Not worth building until there is a second use-case.

### Deliberate consequence: affine packages can stall

If the only worker declaring `unreal-engine` is offline, `unreal-engine` builds
sit in `ENQUEUED` indefinitely rather than being handed to a worker that would
certainly fail. That is the correct trade — but it must be *visible*:

* Liveness is **not** part of the affinity rule (an approved-but-offline worker
  still reserves its packages), because a build handed elsewhere fails rather
  than waits.
* Revoking or deleting the worker releases the reservation — the rule counts
  only *approved* workers, so a status flip is enough.
* A reserved-and-stalled build must say so in the UI, and retiring the worker
  that holds the reservation must be a one-click operation. Both are load-bearing
  for this design rather than polish; they are specified in Part 4.

---

## One package at a time per worker

On top of affinity, architecture and priority, a worker is never handed a build
of a package it is already building. This is not routing policy but a hard
constraint of the caches: `SRCDEST` is keyed by pkgbase and *not* by platform,
since downloads are architecture-independent, so an x86_64 and an aarch64 build
of one package share a directory and would fetch into it at once. Plain sources
are written as `<file>.part` and renamed, keyed by filename, so two such builds
race on the same name.

The worker refuses to run them concurrently regardless
(`aurcache_worker::srcdest_lock`), so offering the job would not make it build
any sooner -- it would sit claimed, holding a concurrency slot and spending its
own `job_timeout`, which the reaper measures from the claim rather than from
when the build actually started. Left queued instead, another worker can take it
at once.

**Per worker, not fleet-wide.** Two workers have separate source caches, and
building a package's platforms simultaneously on different machines is what
multi-arch is for. The predicate is `ACTIVE` alone, like the rest of lease
policing: a build that has reached `PUBLISHING` has ended its lease and its
worker has finished with the sources.

It cannot be the *only* guard, which is why the worker-side lock exists too: a
worker with free slots issues claims concurrently, and a build becomes `ACTIVE`
only when its claim's compare-and-swap wins, so two claims for different
platforms of one package can both pass this filter before either commits. Rare,
and exactly when the mirror must not be fetched twice.

---

## Part 2 — Worker priority

### The pull problem

Workers pull; there is no dispatcher to hand a job to the best worker. Priority
therefore has to be implemented as a **hold-back on the puller**: a lower-priority
worker voluntarily declines a job a better worker could take.

A fixed delay alone ("wait 60 s before a low-priority worker may claim") is a bad
fit for the fallback-worker use case: when the fast workers are saturated, every
job pays the full delay for nothing. So the hold-back is conditioned on whether a
better worker could actually take the job *right now*, with a delay only as a
backstop.

### Rule

Worker `me` may claim build `b` when:

```
capable(me, b) && (!blocked(me, b) || age(b) >= spill_delay)

capable(w, b) = affinity_ok(w, b) && arch_ok(w, b)
available(w)  = live(w) && active_builds(w) < concurrency(w)
live(w)       = w.last_seen >= now - liveness_timeout
blocked(me,b) = ∃ w ≠ me : w.priority > me.priority
                        && capable(w, b) && available(w)
age(b)        = now - b.start_time     // start_time is the enqueue time
```

`priority` is an integer, **higher wins**, default `0`. With every worker at the
default nothing is ever blocked, so existing deployments behave exactly as
today. Equal priorities never block each other — only a *strictly* higher one
does.

This gives the intended behaviour directly:

| Fleet state | Outcome |
|---|---|
| Fast worker idle | Fallback is blocked; fast worker claims on its next poll (≤ `WORKER_POLL_INTERVAL`, default 10 s). |
| Fast worker saturated | `available` is false → not blocked → fallback claims **immediately**, no delay. |
| Fast worker offline | `live` is false → not blocked → fallback claims immediately. |
| Fast worker live, idle, but wedged | Blocked until `age(b) >= spill_delay`, then the fallback takes it. |

The last row is why the delay backstop stays: `available` is inferred from
heartbeats and lease state, and a worker can be healthy by those signals while
never actually claiming (full disk, a bug, an arch mismatch we mis-modelled).
`spill_delay` bounds the damage at one delay per job.

Requeued builds keep their original `start_time`, so a job whose fast worker
died mid-build is instantly claimable by the fallback instead of waiting out
another `spill_delay`. That is the behaviour we want.

### Concurrency reporting

`available` needs to know whether a worker is full, which needs its concurrency
limit. Workers already compute this (`WORKER_CONCURRENCY`, defaulting to
`nproc`); it just is not reported. Add it to `RegisterRequest` and store it.
Active builds per worker come from one grouped count over
`builds WHERE status = ACTIVE`.

### Defaults

| Setting | Where | Default | Notes |
|---|---|---|---|
| `WORKER_PRIORITY` | worker | `0` | Higher wins. Fallback worker: set negative, or raise the fast ones. |
| `WORKER_PACKAGES` | worker | *(empty)* | Comma/space separated exact pkgbase names. |
| `WORKER_SPILL_DELAY` | server | `60` s | Backstop before priority stops mattering. |
| `WORKER_LIVENESS_TIMEOUT` | server | `60` s | ~4× the 15 s default heartbeat. |

Priority and affinity are refreshed on every re-registration, like the arch
lists: the worker's environment is the source of truth and a restart applies
changes — workers re-register on every boot to report their current
configuration (Part 4). **Decided: no server-side override.** AURCache's settings cannot
override environment variables anywhere else, and carving out an exception for
these two would mean the workers page shows a value that may or may not be what
the worker's env says — the ambiguity costs more than the convenience of
retuning without a restart.

---

## Part 3 — Credentials on a worker

Affinity routing is only useful if the affine worker can actually build the
thing. For `unreal-engine` the missing piece is an SSH key reachable from inside
the build chroot: the `git+ssh://` source is fetched by `makepkg` during the
build, not by the server's snapshot store.

### The key itself: provided, or generated as a fallback

Two modes, in precedence order:

1. **`WORKER_GIT_SSH_KEY=/path/to/key`** — use exactly this key, and **do not
   generate one**. This is the zero-touch path: the credential is provisioned
   with the machine, so deployment completes with no manual step afterwards. It
   is the right choice for a fully automated setup, and for a centrally issued
   credential you have no say in.
2. **Self-generated (fallback).** When no key is configured, the worker
   generates an ed25519 keypair into `<data_dir>/ssh/id_ed25519` on first start
   and logs the **public** key at INFO. Onboarding then costs one manual action:
   add that public key to the GitHub account.

Generation happens **only when no key was provided**. Generating a second,
unused keypair alongside a supplied one would waste nothing but would make "which
key is actually in use?" ambiguous at exactly the moment someone is debugging an
authentication failure.

Within mode 2, generate unconditionally rather than only when `WORKER_PACKAGES`
is non-empty — it costs one file and a few milliseconds, and it means the key is
already there the day a package needs it.

Mode 2 should be the documented default, because it is a better fit for GitHub
than distributing a key. A given public key can be attached to only one GitHub
account, but an account may hold many keys — so N workers each holding their own
key is not a workaround, it is the shape GitHub wants. Consequences:

* The private key never leaves the machine that uses it: not in the compose
  file, not in git, not in `docker inspect`, not on the AURCache server.
* Retiring a worker is deleting one public key from GitHub, not rotating a
  shared secret across every machine that had a copy.

Mode 1 trades that away deliberately — one credential, distributed — in exchange
for needing no human in the loop after deployment. Both are legitimate; they
optimise for different things, and which one is right depends on whether you
value blast-radius containment or hands-off provisioning more for that machine.

`<data_dir>` **must** be a persisted volume in mode 2, or the key is regenerated
on every restart and GitHub silently stops accepting it. This is not a new
requirement — the worker's mTLS identity already lives there and already breaks
the same way — but it is now a requirement with a confusing failure mode, so it
belongs in the docs and ideally in a startup warning when `data_dir` looks
ephemeral. Mode 1 is immune, since nothing is generated.

### Supplying a key explicitly

When you do need mode 1, mount it — never pass it through the environment. A PEM
in an env var is exposed via `docker inspect` and `/proc/<pid>/environ`, ends up
in the compose file you commit, and needs base64 wrapping because it is
multi-line. It is both less safe and less convenient than one `volumes:` line:

```yaml
services:
  worker:
    environment:
      WORKER_PACKAGES: unreal-engine
    volumes:
      - ./secrets/epic-ssh:/etc/aurcache-worker/ssh:ro
```

Compose file-secrets are the same mechanism with better ergonomics and are
preferred where available:

```yaml
secrets:
  epic_ssh_key:
    file: ./secrets/id_ed25519
services:
  worker:
    secrets: [epic_ssh_key]
    environment:
      WORKER_GIT_SSH_KEY: /run/secrets/epic_ssh_key
```

### Getting the key into the chroot

`makechrootpkg` already receives bind mounts from `build_command` — the
`-d <src>:<dest>` used for `SRCDEST`. **As implemented**, the worker copies the
key into a per-job staging directory on the host at mode `0600` owned by the
worker's own user, then bind-mounts that directory to `/build-secrets`. Copying
rather than mounting the original is what matters. `makepkg` runs as the unprivileged `builder` user and OpenSSH refuses a
group- or world-readable private key, so a bind-mounted `root:root 0600` file
would be unreadable; copying sidesteps that entirely and lets the host-side file
stay `root:root 0400 :ro`.

The key is read **at job setup, not at worker startup**. Replacing the file on
the host therefore takes effect on the next build with no restart — which is the
whole benefit a runtime key-management API would have offered, without giving the
worker an inbound network surface (see below).

The worker then appends to the `makepkg.conf` it materialises for the job:

```sh
export GIT_SSH_COMMAND="ssh -i /build-secrets/ssh/id_ed25519 -o IdentitiesOnly=yes \
    -o UserKnownHostsFile=/build-secrets/ssh/known_hosts"
```

`makepkg.conf` is sourced by `makepkg`, so the export reaches `git` without
`makechrootpkg` having to forward environment variables. The rendered
`makepkg.conf` arrives from the server in the `JobDescriptor`; the worker
appends its local lines on top, so worker-local secrets never enter the server's
job config.

Ship github.com's host keys in the worker image for `known_hosts`, with
`WORKER_SSH_KNOWN_HOSTS` to point at additional entries. `StrictHostKeyChecking=
accept-new` is the lazier alternative and is fine for a homelab, but a shipped
`known_hosts` costs nothing.

A generic `WORKER_BIND_MOUNTS=host:chroot,…` appending extra `-d` pairs (or `-D`
for read-only, where the installed devtools supports it) covers the same need for
credentials that are not SSH — a `.netrc`, an API token, a licence file.

### Rejected: a key-management API on the worker

The worker today has **zero inbound network surface** — it is a pure outbound
mTLS client. For a machine that holds a build credential and executes arbitrary
PKGBUILDs, that is worth keeping. An inbound listener that accepts private keys
needs its own auth story, its own TLS story, and persists the key to disk
anyway. The only thing it buys over the file is rotation, and reading the key at
job setup time already provides that.

### Rejected: server-held secrets in the `JobDescriptor`

Worth naming because it is the only option that would make affinity unnecessary
for the credential case: store the key in AURCache, ship it per-job over the
existing mTLS channel, configured once in the UI. Rejected because it puts a
private key in the server database and hands it to whichever worker claims the
job — converting "one machine holds the key" into "every approved worker holds
the key" — and because it does nothing for the non-credential reasons to want
affinity (scratch disk, a licensed toolchain).

### Other things this class of package needs

`WORKER_BUILD_TIMEOUT` defaults to 3 hours; an Unreal build will exceed that
comfortably. Disk headroom is worth a mention in the docs too.

---

## Part 4 — Making a reservation visible and reversible

A hard routing rule that can stall is only acceptable if an operator can see the
stall and undo it in one action. Neither is true today.

### Explaining a stalled build

The server should say *why* an `ENQUEUED` build is not moving. The fleet snapshot
the claim path already builds has everything needed, so this is a small addition:
compute a `waiting_reason` for `ENQUEUED` builds only, and surface it on the
build API model.

```jsonc
// none                      -> genuinely just queued
{ "kind": "affinity", "workers": ["epic-box"], "any_live": false }
{ "kind": "arch",     "arch": "aarch64" }   // no approved worker builds it
```

The `arch` variant covers an existing blind spot: an `aarch64` build with no
`aarch64` worker enrolled stalls today with no explanation either.

In the UI, an `ENQUEUED` build with a reason renders as *"Waiting for worker
**epic-box** (offline) — package affinity"* rather than a bare "enqueued", and
the workers page shows a warning when an approved worker holding reservations
has been offline past some threshold. Without this, a reserved-and-stalled build
is indistinguishable from a merely slow queue, which is the single most likely
way this feature wastes someone's afternoon.

Cost is two small queries on the builds-list endpoint, and only when an
`ENQUEUED` build is present.

### Retiring a worker

`worker_store.rs` has approve / revoke / list. Revoke already releases affinity
reservations for free, since the rule counts only *approved* workers. What is
missing is not a delete — worker rows are **kept forever**, so a build from two
years ago can still show which machine produced it and with what arches and
version. A `workers` row is a few hundred bytes; build history that resolves is
worth more. This also means `builds.worker_id` always resolves and there is no
dangling-reference case to design around.

So "retire" is not a new concept: **retiring a machine is revoking it**, and
re-allowing it is approving it again. The row, the certificate, and the history
all persist across both. Three gaps to close:

* **Revoke does not requeue in-flight work.** A worker revoked mid-build leaves
  its builds `ACTIVE` until the lease reaper notices, up to `LEASE_TTL` (60 s).
  Revoke should run the existing `requeue_or_fail` over that worker's active
  builds immediately — the machinery exists, it just is not called from here.
* **The list grows without bound.** Keeping rows forever is right for history
  and wrong for the default view. The workers page should hide revoked workers
  behind a `Show retired (N)` toggle rather than listing every machine ever
  enrolled.
* **A returning machine is invisible** — see below.

### Registration on every boot

**Decided: the worker re-registers on every startup.** Registration is the
channel through which a worker states its current configuration, and all of it —
name, arches, concurrency, priority, package affinity, SSH public key, version —
can differ from the last time it ran. Treating registration as a once-per-machine
event makes the server's picture of the fleet drift from reality with every
config edit.

This is safe and idempotent as the code already stands. `Identity::load_or_create`
persists the key and `generate_csr` signs with it, so the SPKI fingerprint is
stable across restarts (`identity.rs` already tests this). Re-registering hits
the existing `ON CONFLICT (cert_fingerprint)` path: same row, no churn, approval
state preserved.

Three consequences to handle:

* **`ensure_enrolled` must stop short-circuiting.** It currently returns early on
  `identity.is_enrolled()` (`backend/aurcache-worker/src/enroll.rs:32`) and never
  calls `/worker/register`. The new flow always registers; the only difference
  between an enrolled and a fresh worker is where the CA comes from (persisted
  via `identity.ca_pem()` versus pinned by fingerprint or TOFU) and whether the
  worker must then poll for approval. Reusing the persisted CA is also strictly
  better than re-running trust-on-first-use.

* **Re-registration must be best-effort when a valid certificate is already
  held.** The bundled topology boots the worker and the backend from the same
  `docker compose up`, so a worker will regularly try to register before the
  backend is listening. A worker that already has a certificate must log the
  failure, proceed with its persisted config, and retry in the background —
  never refuse to start. Only a worker with no certificate blocks on enrollment,
  which it already does.

* **`register_worker` must refresh `name`.** The `ON CONFLICT` update currently
  touches `native_arches`, `emulated_arches`, `last_seen`, and `version`, so a
  changed `WORKER_NAME` silently does not propagate. Add `name` alongside the
  new columns. The issued certificate's subject then no longer matches the
  worker's name, which is harmless — identity is mapped by SPKI fingerprint, not
  subject — but is worth a comment so nobody "fixes" it by reissuing certs.

Registration stays boot-only. Its payload is sourced entirely from environment
variables, which cannot change without a restart, so periodic re-registration
would carry no new information; `heartbeat` remains purely a liveness and lease
signal.

### A revoked worker that comes back

Registering on every boot gives this case its signal for free: a returning
machine calls `/worker/register`, which refreshes `last_seen` without touching
approval state, so the workers page can show *"revoked — checked in 30 s ago"*.
That is what an operator needs in order to click Approve. A worker that is
refused at the job endpoints should additionally re-enter the enrollment loop on
a `403` rather than spinning on refused claims, so a machine revoked *while
running* behaves the same as one revoked between boots.

**This makes fixing auto-approval mandatory.**

For context: revocation is *not* cryptographic. There is no CRL — the
`aurcache-ca` crate has no revocation function, and the mTLS listener is
configured with nothing but the CA bytes
(`MutualTls::from_bytes(&ca_pem).mandatory(false)`, `init.rs:70`). After a
revoke, the worker's leaf certificate is still valid, still chains to the CA,
still sits on its disk and in `workers.signed_cert`, and still completes the TLS
handshake. All that changes is that `WorkerAuth` refuses to map it to an
authorized worker, because `revoke_worker` flipped one column. **The `status`
column is the entire enforcement mechanism**, so any write of `status =
approved` is a complete, immediate re-grant — no re-keying, no new CSR, the
worker already holds everything it needs.

That makes the auto-approve gate security code rather than convenience code, and
it is currently wrong:

```rust
if worker.status != WorkerStatus::APPROVED && auto_approve_from_env(…) {
    worker_store::approve_worker(db, worker.id).await?;
}
```

`!= APPROVED` reads as "is pending", but the domain has three values, so it also
matches `revoked`. Each auto-approve mode is durable ambient policy that
revocation cannot touch: the worker itself wrote `<fingerprint>.csr` into the
shared enrollment volume and revoking does not delete it; its fingerprint is
still listed in `AURCACHE_PREAPPROVED_WORKERS`; it still holds
`AURCACHE_ENROLLMENT_TOKEN` in its own environment. Revoke is meant to be a
per-machine override of enrollment policy; instead the policy overrides the
revoke, and `register_status_for` then hands back the *same* cached certificate.

Today the blast radius is limited by `ensure_enrolled` only registering when it
has no certificate on disk:

| Scenario | Today |
|---|---|
| Approved worker revoked while running, then restarted | Holds — `is_enrolled()` is true, so it never re-registers |
| Worker revoked while still `pending` | **Resurrects.** It exits with "worker was revoked by the server", its supervisor restarts it, it registers again, and is auto-approved |
| Certificate lost but key kept (partial volume, bad upgrade) | **Resurrects** — same fingerprint, no cert |
| Whole `data_dir` wiped | Unaffected — new key, new fingerprint, new `pending` row (see ghost reservations below) |

The second row is the case revoke exists for, and it fails within seconds of the
click. Once every boot re-registers, the precondition becomes simply *"the
worker restarted"* and the first row fails too: **revocation would hold only
until the machine reboots**, while still appearing to work at the time it was
clicked.

> **Implemented:** narrow the gate to `status == PENDING` (`eval_auto_approve`). Auto-approval answers
> "should I trust a machine I have never seen?"; it must never answer "should I
> re-trust a machine an operator explicitly denied?". A returning retired machine then re-registers, refreshes
> `last_seen`, and waits as `revoked` until someone clicks Approve — one click,
> with the row and certificate still on file and no re-enrollment.

The alternative, if a retired machine should come back *automatically*, is to
split the status: `retired` (auto-approves on return) versus `revoked` (never
does). That keeps a compromised worker permanently denied, which a single
combined status cannot. It costs a status value and a second button; worth it
only if machines cycle often enough that the click becomes a chore.

### Ghost reservations after a wiped `data_dir`

Registration is keyed on the worker's persisted key. Recreate a worker container
without a volume for `data_dir` and it generates a *new* key, so it registers as
a **new** `pending` worker rather than the existing one. The old row stays
`approved` and — per Part 1 — keeps reserving its affinity packages forever,
while the machine that actually holds the SSH key now sits unapproved under a
different id. The affine package stalls with no obvious cause.

This is the same `data_dir` persistence requirement as the SSH key in Part 3 and
the mTLS identity before it, but with a worse failure mode, because the stall is
silent. The Part 4 warning — an approved worker holding reservations that has
been offline beyond a threshold — is what surfaces it, and revoking the ghost row
is the fix. Worth an explicit callout in the docs next to the volume
requirement.

---

## Implementation

### Data model

New migration, `backend/aurcache-db/src/migration/m2026xxxx_worker_routing.rs`,
adding to `workers`:

| Column | Type | Default | Notes |
|---|---|---|---|
| `package_affinity` | `TEXT NOT NULL` | `''` | Comma-separated exact pkgbase names, matching the existing `native_arches` encoding convention. |
| `priority` | `INTEGER NOT NULL` | `0` | Higher wins. |
| `concurrency` | `INTEGER NOT NULL` | `1` | Reported at registration. |
| `ssh_public_key` | `TEXT` | `NULL` | Worker's build-credential public key, for display. |

Defaults are chosen so an un-upgraded worker keeps working unchanged: empty
affinity reserves nothing, priority 0 blocks nobody, and a worker that reports no
public key simply shows none.

Mirror the columns on `backend/aurcache-db/src/workers.rs`.

### Wire protocol (`backend/aurcache-common/src/worker.rs`)

`RegisterRequest` gains four `#[serde(default)]` fields — `packages:
Vec<String>`, `priority: i32`, `concurrency: u32`, `ssh_public_key:
Option<String>` — so an old worker against a new server still enrolls.

`ClaimRequest` is **not** extended. Affinity and priority are read from the
stored `workers` row, never from the request: every worker's decision depends on
every other worker's declared values, so they must come from one consistent
source. (The existing arch fields on `ClaimRequest` are redundant with the row
for the same reason, but changing that is out of scope here.)

### Claim algorithm (`backend/aurcache-db/src/helpers/worker_jobs.rs`)

Restructure `claim_job` from the current two nested passes into a single scored
scan, which is both simpler and where the new rules fit naturally:

```rust
struct Fleet {                        // one snapshot per claim call
    workers: Vec<WorkerCap>,          // approved: id, priority, concurrency,
                                      // arches, affinity names, last_seen
    active: HashMap<i32, i32>,        // worker_id -> ACTIVE build count
    native_arches: HashSet<String>,   // arches reserved to a native worker
}

// 1. Load fleet snapshot: approved workers + grouped ACTIVE build counts.
// 2. Load ENQUEUED candidates joined to packages -> (build_id, pkgname,
//    platform, start_time).
// 3. Build the affinity index straight from the worker rows:
//        affinity: HashMap<&str, HashSet<i32>>   // pkgname -> claiming workers
//    One pass over every worker's declared names; independent of candidates.
// 4. Filter to capable(me, b).
// 5. Drop b where blocked(me, b) && age(b) < spill_delay.
// 6. Sort by (affine_to_me desc, native_to_me desc, start_time asc).
// 7. Walk the sorted list, attempting the existing conditional
//    `UPDATE ... WHERE status = ENQUEUED` CAS until one wins.
```

Step 3 is what keeps the predicates O(1). Because affinity entries are exact
names, the index is a direct inversion of the worker rows — one pass over every
declared name, with no matching to perform and no dependency on which builds are
queued. It turns three separate questions into hash lookups:

```rust
reserved(p)   = affinity.get(p).is_some_and(|ws| !ws.is_empty())
affine(w, p)  = affinity.get(p).is_some_and(|ws| ws.contains(&w))
capable(me,b) = arch_ok(me, b) && (!reserved(b.pkg) || affine(me, b.pkg))
```

`blocked(me, b)` needs `affine(w, b.pkg)` for every *other* higher-priority
worker, which is why the index maps to a **set of worker ids** rather than a bare
`reserved: HashSet<String>`. Building it costs one pass over the total number of
declared affinity entries across the fleet — a handful of strings.

**Not a long-lived cache.** The fleet splits into a stable half (affinity,
priority, concurrency, arches — changes only on re-register/approve/revoke) and a
volatile half (`last_seen`, active build counts — changes on every heartbeat and
every claim). `available(w)` needs the volatile half, so a database round trip
happens on every claim regardless; caching the stable half separately saves
nothing while adding invalidation hooks on approve, revoke, and re-register,
each a chance to route a job to a worker that cannot build it. The call rate does
not justify the risk either: 20 workers polling every 10 s is ~2 claims/sec
against a table with a handful of rows.

Step 6's first key is the one non-obvious addition: a worker that *is* affine for
a package should take that job before a job anyone could have taken, because it
may be the only worker that can. Steps 1–2 are two extra small queries per claim
(`workers` is tiny; enqueued builds are few) and step 7 keeps the existing
race-free hand-off unchanged.

The rules belong in the db helper, not the API layer, so they stay unit-testable
against `sqlite::memory:` like the current claim tests.

### Touch list

| File | Change |
|---|---|
| `backend/aurcache-db/src/migration/m2026xxxx_worker_routing.rs` | new columns (+ `mod.rs` registration) |
| `backend/aurcache-db/src/workers.rs` | entity fields |
| `backend/aurcache-db/src/helpers/worker_jobs.rs` | fleet snapshot, `capable`/`available`/`blocked`, rewritten `claim_job` |
| `backend/aurcache-common/src/worker.rs` | `RegisterRequest` fields |
| `backend/aurcache-api/src/worker.rs` | pass new fields through register; `spill_delay`/`liveness_timeout` env accessors |
| `backend/aurcache-api/src/build.rs` | `waiting_reason` on `ENQUEUED` builds |
| `backend/aurcache-db/src/helpers/worker_store.rs` | refresh `name`, affinity, priority, concurrency, SSH key on re-register; requeue active builds on revoke |
| `backend/aurcache-api/src/worker_enroll.rs` | auto-approve gated to `pending` only |
| `backend/aurcache-worker/src/config.rs` | `WORKER_PACKAGES`, `WORKER_PRIORITY`, `WORKER_BIND_MOUNTS`, `WORKER_GIT_SSH_KEY`, `WORKER_SSH_KNOWN_HOSTS` |
| `backend/aurcache-worker/src/identity.rs` | generate + persist the build SSH keypair alongside the mTLS identity |
| `backend/aurcache-worker/src/enroll.rs` | always register on startup (best-effort when already enrolled); re-enter enrollment on 403; send all config incl. the SSH public key |
| `backend/aurcache-worker/src/build.rs` | extra `-d` binds; copy key into the job chroot; `makepkg.conf` append |
| `frontend/lib/models/worker.dart`, `screens/workers_screen.dart` | priority, affinity, SSH public key + copy button, `Show retired` toggle |
| `frontend/lib/components/…` build list | render `waiting_reason` |
| `docs/docs/…` | worker configuration reference; the "add this key to GitHub" walkthrough |

### Tests

New cases in `worker_jobs.rs`, all against in-memory SQLite in the style of the
existing ones:

* affine package is refused to a non-affine worker, granted to the affine one;
* a package no worker claims is unaffected by the presence of affinity lists;
* affine worker prefers its affine job over an older general job;
* affinity matches exactly: a worker claiming `unreal-engine` does not reserve
  `unreal-engine-bin` or `unreal`;
* offline-but-approved affine worker still reserves; revoked one does not;
* low-priority worker is blocked while a live, non-full, higher-priority worker
  is capable;
* …and claims immediately when that worker is at `concurrency`, or stale;
* …and claims anyway once `age >= spill_delay`;
* equal priorities do not block each other;
* affinity and arch reservation compose (affine + foreign arch);
* revoking a worker releases its reservations and requeues its active builds;
* re-registering a revoked worker refreshes its config and `last_seen` but
  leaves it revoked — including when every auto-approve mode matches;
* re-registering an approved worker with changed name / arches / priority /
  affinity updates the row in place and keeps it approved;
* a `pending` worker is still auto-approved by each of the three modes
  (the fix must not break first-contact enrollment).

For the worker side, the existing pure-helper style covers: key generated only
when absent, an explicitly configured key taking precedence, and the
`GIT_SSH_COMMAND` line appearing in the rendered `makepkg.conf`. The chroot copy
(mode/uid) needs a real chroot and is validated in the e2e pass instead.

### UI

`workers_screen.dart` gains a priority column, affinity chips, the worker's SSH
public key with a copy button, and a `Show retired (N)` toggle that hides revoked
workers from the default view — there is no delete action, since rows are kept
forever so build history keeps resolving (Part 4). The builds list renders
`waiting_reason` (Part 4). `worker.dart` is hand-written, so
no `build_runner` pass is needed for the model.

### Status

Implemented (2026-08-24):

* the migration, entity fields, and `RegisterRequest` plumbing (step 1);
* the `claim_job` rewrite to a scored scan over a per-claim fleet snapshot,
  with the affinity index and the `capable`/`available`/`blocked` predicates
  (steps 2–4);
* `WaitingReason` and its exposure on the builds API;
* revoke now releases reservations *and* requeues the worker's in-flight builds;
* workers re-register on every boot, best-effort when already enrolled;
* the workers page (priority, affinity chips, `Show retired` toggle) and the
  stalled-build warning in the builds table;
* the auto-approve gate and the certificate cleanup (landed 2026-08-23).

* the worker-side build credential (step 6): `credentials.rs` resolves an
  explicit `WORKER_GIT_SSH_KEY` or generates an ed25519 key under
  `<data_dir>/ssh`, logs the public half on every start, stages a `0600` copy
  per job, exposes it via a `makechrootpkg -d` bind mount, and appends
  `export GIT_SSH_COMMAND=…` to the job's `makepkg.conf`. `openssh` was added to
  the worker image (for `ssh-keygen`) and to the base chroot package set (for
  `git+ssh` fetches, which `base-devel` alone cannot do).

User-facing docs (step 7) are in `docs/docs/workers/routing.md`.

**Step 6 is validated end-to-end** by `scripts/test-e2e-ssh.sh`, which stands up
a throwaway git server, authorises a per-run keypair, and builds a fixture whose
source is fetched over `git+ssh`. It runs the build twice — once without the
credential, asserting failure — so a pass cannot come from a cached source or a
skipped fetch, and checks that a marker from the SSH-only repository ends up
inside the built package.

That test changed the design. Three things were wrong, and none was reachable by
unit tests:

* **Sources are fetched outside the chroot.** `makechrootpkg`'s
  `download_sources()` runs `makepkg --verifysource -o` on the *worker*, as the
  build user, before the container is entered — using the chroot's
  `makepkg.conf` but the worker's filesystem. A credential bind-mounted into the
  chroot is therefore invisible at exactly the moment the fetch happens.
* **The credential path must be stable across jobs.** `GIT_SSH_COMMAND` is
  written into the *base* chroot's `makepkg.conf` when that chroot is created,
  and every later build reads a copy of it, so a per-job path would be correct
  for the first build and stale for every one after.
* **The credential must not be exposed to the chroot at all.** Everything a
  PKGBUILD runs — `prepare`, `build`, `package` — executes in there, so anything
  reachable can be exfiltrated by a hostile package. Since the fetch already
  happened on the worker, exposing it buys nothing.

The key is therefore staged once at `<data_dir>/secrets`, never bind-mounted
into the chroot, and the `makepkg.conf` export is **guarded** on the key being
readable:

```sh
if [ -r "/var/lib/aurcache-worker/secrets/id_ed25519" ]; then
    export GIT_SSH_COMMAND="ssh -i … -o IdentitiesOnly=yes …"
fi
```

One file serves both environments: the export applies on the worker, and inside
the chroot it simply does not. Without the guard, ordinary git operations in a
PKGBUILD would be handed `-i <missing file> -o IdentitiesOnly=yes`, which also
suppresses any identity they would otherwise have used — breaking git operations
that have nothing to do with our credential.

The uid question this section previously flagged turned out fine:
`makechrootpkg` maps `builduser` to the same uid the worker runs as, so the
staged `0600` key is readable where it needs to be. That only matters for the
`-d` binds that remain (the per-job pacman cache), not for the credential.

Two decisions taken during implementation that the design did not specify:

* **`concurrency` is not enforced server-side.** It feeds `available()` only.
  The worker already bounds its own parallelism with a semaphore before
  claiming, and it is authoritative about its real capacity — a stale reported
  number must not be able to deadlock a worker out of claiming its own work.
* **`ClaimRequest` is still accepted but ignored.** Removing it would break the
  wire protocol for no gain; the handler documents that routing reads the stored
  row instead.

### Suggested order

1. Migration + entity + register plumbing (no behaviour change).
2. `claim_job` rewrite to the scored scan, with existing tests still green.
3. Affinity rule + tests.
4. Priority rule + tests.
5. Revoke-requeue, worker re-registration, `waiting_reason`, and the UI — the
   safety net for step 3, so it should not lag far behind it. (**The
   auto-approve gate is already implemented**; re-registration was coupled to it
   and is the remaining half — a revoked worker that re-registers now stays
   revoked, but nothing yet makes it re-register.)
6. Worker-side SSH keypair, chroot copy, `GIT_SSH_COMMAND`, validated against a
   real chroot.
7. Docs.

Steps 3 and 4 are independent and can land in either order. Step 6 is the only
one that cannot be finished without a real worker.

---

## Open questions

* **Should affinity ever be soft?** Currently no: affinity means "only these
  workers", and preference is expressed with priority instead. A soft variant
  would need a second list and a third routing tier; the split in this document
  exists precisely to avoid that.
* **Does `unreal-engine` need more than the key?** The PKGBUILD also expects the
  GitHub account to be a member of the Epic Games organisation, which is a
  property of the account the key is attached to, not of the worker. Worth
  confirming against a real build that the key alone is sufficient before
  writing the walkthrough.
* **Warning threshold for a stalled reservation.** Part 4 flags an approved
  affinity-holding worker that has been offline "past some threshold". Reusing
  `WORKER_LIVENESS_TIMEOUT` (60 s) would be noisy for a laptop that is simply
  off; something on the order of an hour is probably right, but it is a guess
  until the feature has been lived with.

* **Automatic or one-click re-allow?** Part 4 proposes that a returning retired
  machine waits for an Approve click. Making it automatic requires splitting
  `retired` from `revoked` so a compromised worker stays denied.

## Appendix — What the CA actually secures

This is not part of the routing design, but Part 4 leans on it and the current
code invites the wrong reading, so it is worth stating outright.

### The model is `authorized_keys`, not PKI

A worker's identity is `SHA-256(SubjectPublicKeyInfo)` of a keypair it generates
once and persists. Authorization is a row in the `workers` table that an
operator controls. Two checks run on a job request, and the second subsumes the
first:

* **TLS** — the presented client certificate chains to AURCache's CA.
* **`WorkerAuth`** — the certificate's SPKI fingerprint maps to a row with
  `status == APPROVED`.

To pass the second check you must hold the private key whose fingerprint is
registered and approved, and TLS client authentication already proves possession
of that key *regardless of who signed the certificate*. Accept any client
certificate instead of CA-signed ones and the authorization decision is
identical. **The CA signature adds no authentication strength over the
fingerprint lookup.**

Three facts in the current code confirm this rather than contradict it:

* `WorkerAuth` (`backend/aurcache-api/src/worker.rs:100`) is the only consumer of
  the client certificate, and it reads exactly one field — `subject_pki.raw`.
  Subject, issuer, serial, extensions and validity are never consulted.
* `workers.cert_serial` and `workers.not_after` were written by
  `store_signed_cert` and **read by nothing** — the fields a CRL or an expiry
  policy would need, neither of which exists. `cert_serial` has since been
  removed (see below).
* `register_worker` signs every CSR that arrives, before any trust decision —
  the `if worker.signed_cert.is_none()` branch does not consult `status`. A CA
  signature therefore attests only that *a CSR reached the endpoint*.
  Certificates are minted for workers that are pending and would be minted for
  one revoked a second later.

What the CA does earn is the **other** direction: workers pin it
(`fetch_and_pin_ca`, `AURCACHE_SERVER_CA_FINGERPRINT`) to verify they are
talking to the real AURCache, and it encrypts the channel. That is genuine
anti-MITM protection, and it is the CA's actual job here despite the naming
suggesting otherwise.

### Consequences, stated so they are not re-litigated

* **Trust is the database row.** Defending against an attacker who can write to
  AURCache's database is explicitly **out of scope** — consistent with the
  existing "approved workers are trusted by design" boundary in
  `remote-workers.md`.
* **Revocation as a status flip is correct and complete.** It is not
  cryptographic, it does not need to be, and a CRL would buy nothing under the
  scope above. Do not add one.
* **The auto-approve gate is the whole security control.** Since the DB row is
  the trust anchor and any write of `status = approved` is an immediate,
  complete re-grant to a worker that already holds a valid certificate, the
  `status == PENDING` narrowing in Part 4 is not a tidy-up. It is the only thing
  standing between an operator's decision and its silent reversal.
* **Releasing the certificate only on approval implies more than it delivers.**
  Holding a signed certificate does not mean approved, because the guard
  re-derives trust from the row on every request. Keep the behaviour — it costs
  nothing — but do not treat certificate possession as evidence of anything.

### The CA's real job is late binding, not trust

There is a structural reason the CA exists, separate from the trust semantics its
name implies, and it is easy to miss.

Rocket builds its client trust anchors **once, at listener bind time**.
`load_ca_certs` (`rocket_http-0.5.1/src/tls/util.rs:49`) simply adds every
certificate in the configured PEM to a `RootCertStore`; it has no concept of "a
real CA". The verifier is then fixed for the life of the listener
(`tls/listener.rs:88-90`, `AllowAnyAnonymousOrAuthenticatedClient::new(ca)`),
and Rocket exposes no hook to supply a custom `ClientCertVerifier` or to reload
anchors on a running server.

So self-signed worker certificates are not *rejected as such* — one would verify
fine if it were in the root store. They are impossible because the store cannot
learn about a worker that enrolls after launch. Rebinding the listener on every
approval would drop in-flight worker connections (log streams, artifact
uploads), and there is no graceful TLS reload.

**The CA is what resolves that.** It lets the server commit to a single trust
anchor at startup for identities that do not exist yet, so workers can enroll
dynamically without a restart. That is real engineering value and the honest
answer to "why is there a CA at all" — it is just an entirely different reason
from the one the naming suggests. It provides *late binding*, not trust.

This leaves exactly two coherent positions, with nothing useful in between:

* **Keep mTLS** — the CA stays for late binding, certificates are plumbing:
  validity raised to the server's 3650 days, `cert_serial` dropped,
  authorization documented as the database row. Cheap, and now justified rather
  than merely tolerated.
* **Drop client certificates entirely** — anonymous TLS for every request (the
  path enrollment already uses today, since `WorkerClient::enrollment` presents
  no client certificate at all), with worker identity carried at the application
  layer by a token issued on approval. Rocket's constraint stops applying
  because Rocket is no longer asked to authenticate anyone. This removes CSRs,
  signing, `signed_cert`, `not_after`, `cert_serial`, and the expiry/renewal
  problem, at the cost of a replayable bearer credential on the wire and a
  protocol migration.

Note that the first registration works today **not** because unsigned client
certificates are accepted, but because the worker presents *no* certificate:
`AllowAnyAnonymousOrAuthenticatedClient` permits an anonymous client while still
requiring any certificate that *is* presented to chain to the anchors.

### Resolved: workers no longer brick after 365 days

**Implemented.** Certificates were issued with a 365-day
`WORKER_CERT_VALIDITY_DAYS` and there was no renewal path at all:

* once `not_after` passed, the TLS handshake failed;
* `ensure_enrolled` saw `is_enrolled() == true` — both files still on disk — so
  it never re-enrolled;
* and even when it did register, the server only signs when
  `signed_cert.is_none()`, so nothing was ever reissued.

The fix follows from the trust model rather than fighting it. A worker
certificate grants nothing on its own — authorization is the `workers` row — so
there is no security value in a short lifetime, and therefore no renewal
machinery worth building. The default is now **3650 days**, matching the server
certificate. `WORKER_CERT_VALIDITY_DAYS` still overrides it for anyone who wants
PKI-style rotation.

Building renewal instead (reissue when within N days of `not_after`, worker
re-stores the certificate it already receives from `register_status_for`) was
considered and rejected: it is real machinery to keep a non-credential fresh.
That option remains open if worker certificates ever become authoritative.

`cert_serial` has been **removed** — column, entity field, `store_signed_cert`
parameter, and the `SignedWorkerCert.serial_hex` producer in `aurcache-ca` with
its `serial_from_cert_pem` helper. Its only plausible reader was a CRL, which
the trust model rules out; leaving a producer with no consumer is what created
this confusion in the first place. `not_after` stays: it is enforced by TLS even
though nothing in AURCache reads it.

The `workers` table is created by an unreleased, branch-only migration
(`m20260818_000000_remote_workers`), so the column was dropped from that
migration in place rather than adding a drop-column migration for something that
never shipped. **A development database that already applied it keeps a stray
nullable `cert_serial` column** — harmless, since SeaORM only selects mapped
columns, but it will differ from a freshly migrated database.

## Decided

* **No server-side override of worker configuration** (priority, affinity).
  Environment variables are not overridable elsewhere in AURCache and these are
  not an exception. See Part 2 — Defaults.
* **Self-generated SSH key is the default**, with an explicitly supplied key as
  an override; never an environment variable, never an inbound API on the
  worker. See Part 3.
* **Affinity ignores liveness** — an approved-but-offline worker keeps its
  reservation, because handing the job elsewhere fails rather than waits. The
  cost is paid down by Part 4 rather than by weakening the rule.
* **Worker auth stays mTLS; a bearer-token scheme is not built.** The main
  topology is a trusted LAN plus VPN, where the worker listener needs no reverse
  proxy at all — the plane separation (`:8080` human API, `:8083` workers) means
  nginx in front of the UI never touches worker traffic. Long-distance workers
  do not change this: a VPN extends the trusted network unchanged, `:8083` can
  be exposed directly (an attacker without a certificate cannot even complete
  the handshake), and a hard single-public-port constraint is met by nginx
  `stream` + `ssl_preread`, which routes by SNI *without* terminating TLS and so
  keeps mTLS intact. Terminating mTLS at nginx and forwarding identity in a
  header is rejected outright: it replaces a cryptographic check with "the
  backend is unreachable except via nginx", and header spoofing then
  impersonates any worker.

  Should a token ever be needed, adding one is **additive, not a migration**:
  the listener is already `mandatory(false)` so it accepts certificate-less
  connections today; `WorkerAuth`'s existing no-certificate `Outcome::Forward`
  branch becomes "try a bearer token"; both paths resolve to the same `workers`
  row, leaving authorization, revocation and routing untouched. Cost is one
  `token_hash` column, issue-on-approval, and one fallback arm — and the two
  mechanisms coexist on the same listener, so LAN workers keep mTLS while a
  remote worker uses a token. Trigger: public exposure *and* a single-port
  constraint *and* no VPN.

  The trade being accepted meanwhile: a bearer token is replayable and visible
  to every TLS-terminating hop, whereas an mTLS private key never transits. That
  matters because a worker credential can claim a build and upload artifacts
  into the pacman repo. Revocation is identical either way — it is the same
  database status flip — so the difference is purely in how likely the
  credential is to leak, not in how fast one can respond.
* **The CA authenticates the server, not the worker.** Worker trust is the
  database row, keyed by public-key fingerprint; defending against writes to
  AURCache's own database is out of scope. Revocation stays a status flip and no
  CRL is wanted. See the Appendix.
* **Workers re-register on every boot**, reporting their full current
  configuration; registration is the fleet's configuration channel, not a
  once-per-machine event. See Part 4.
* **Worker rows are never deleted.** Build history must keep resolving to the
  machine that produced it; retiring is revoking. See Part 4.

---

## Configuration reference (implemented)

### Worker

| Variable | Default | Meaning |
|---|---|---|
| `WORKER_PACKAGES` | *(empty)* | Comma/space separated **exact** pkgbase names this worker is provisioned for. Any package listed by an approved worker can only be built by workers that list it. |
| `WORKER_PRIORITY` | `0` | Higher wins. A worker holds back only while a *strictly* higher-priority worker is live and has capacity. |
| `WORKER_CONCURRENCY` | `nproc` | Already existed; now also reported so the server can tell whether this worker is full. |

### Server

| Variable | Default | Meaning |
|---|---|---|
| `WORKER_SPILL_DELAY` | `60` s | How long a build waits before priority stops holding it back. |
| `WORKER_LIVENESS_TIMEOUT` | `60` s | How stale `last_seen` may be before a worker stops counting as available. |

### Worked example: fast LAN workers with a colocated fallback

```yaml
# in the same compose file as the backend — always up, slow
worker-fallback:
  environment:
    WORKER_PRIORITY: "-10"

# on a fast machine elsewhere on the network
worker-fast:
  environment:
    WORKER_PRIORITY: "10"
    WORKER_CONCURRENCY: "16"

# the machine holding the Epic Games SSH key
worker-epic:
  environment:
    WORKER_PRIORITY: "10"
    WORKER_PACKAGES: "unreal-engine"
```

`unreal-engine` now only ever goes to `worker-epic`. Everything else prefers the
fast workers, falling to `worker-fallback` the moment they are full or absent —
with no delay in either case, because the fallback only holds back while a
faster worker is *actually able* to take the job.
