# Design: Remote Build Workers

Status: **Implemented** · Last updated: 2026-09-22

## Summary

Replace AURCache's current in-process Docker/Podman builder with **remote build
workers**. AURCache becomes a pure **queue + ingest + repo/source server**:
it keeps the list of build requests (already the `builds` table), and long-lived
worker processes **poll** for jobs, **download** the package source from
AURCache, **build** each package in its own `devtools` chroot, then **upload**
the resulting packages back to AURCache, which adds them to the pacman repo.

This is a **full replacement** — the Docker build path is removed. There is only
one build mode afterwards.

### Goals

- Long-lived worker process that builds each package in its own chroot.
- **Turnkey single-host** setup: `docker compose up -d` with zero edits, zero
  secrets, zero approval clicks (the most common use-case).
- **Remote foreign-arch workers** (aarch64/armv7) that build natively, with
  their arch's jobs prioritized to them.
- **Security via allow-listing**: workers must be explicitly approved; an
  on-network attacker cannot impersonate a worker or inject packages.
- Easy to test; the e2e script works against the new architecture.

### Non-goals (explicit, documented)

- A **compromised/malicious approved worker** — we trust approved workers by
  design.
- **Content authenticity** of built binaries (no reproducible builds).
- **Package signing** — not implemented now; a clean future add-on.

The security boundary is therefore: **mTLS identity + admin approval/allow-list
+ claim-bound uploads**. For a given `pkgname` the server cannot distinguish a
legitimate binary from a malicious one without signing/reproducible builds, so
trust rests on *which* worker is allowed to talk to us, not on inspecting
contents.

---

## Current architecture (baseline)

- Builds live in the `builds` table: `ENQUEUED → ACTIVE → SUCCESSFUL/FAILED`
  (plus `WAITING_FOR_DEPS`). One pending build per `(pkg_id, platform)`
  (partial unique index).
- An in-process `tokio::broadcast::Sender<Action>` drives `init_build_queue`,
  which spawns a `Builder` per job (semaphore-bounded concurrency).
- `Builder` (crate `aurcache-builder`) talks to a **local Docker/Podman socket**:
  pull image → create container → upload source tar from `SnapshotStore` → run
  `makepkg` → wait → `move_and_add_pkgs` copies `*.pkg.tar.zst` into
  `./repo/<platform>/`, runs `repo_add`, updates the `files` table.
- Sources come from `SnapshotStore.archive_bytes()` (AUR / git / upload, with
  patches applied server-side).
- Two build modes exist today (`host` vs `dind`) purely to give the builder a
  Docker daemon — both are removed.
- HTTP API + token auth on **:8080**; pacman repo served on **:8081**.

> **Transport update (implemented):** the worker protocol runs on its **own
> dedicated listener** — HTTPS + mutual TLS on a configurable port (default
> **:8083**, `AURCACHE_WORKER_PORT`) — kept separate from the human/tooling HTTP
> API on **:8080**. This means the UI/API/CLI stay plain HTTP and need no TLS of
> their own (front them with a reverse proxy if you want TLS there), while the
> machine-to-machine mTLS trust model is scoped to exactly the `/api/worker/*`
> surface. The worker *admin* endpoints (`/api/workers`, approve/revoke) use
> operator/session auth and therefore live on the **:8080** plane, not the mTLS
> port.

---

## Target architecture

```
┌──────────── AURCache server ────────────┐        ┌──── remote worker (long-lived) ────┐
│ builds queue (DB)                        │        │ loop:                              │
│ POST /api/worker/jobs/claim  ───────────►│◄───────│  claim job (arch match)            │
│ GET  /api/worker/jobs/{id}/source ──────►│───────►│  download source (server-patched)  │
│ POST /api/worker/jobs/{id}/logs   ◄──────│◄───────│  build in own devtools chroot      │
│ POST /api/worker/jobs/{id}/artifacts ◄───│◄───────│  upload *.pkg.tar.*                │
│ POST /api/worker/jobs/{id}/complete ◄────│◄───────│  report success/fail               │
│ worker mTLS listener :8083 ──────────────│◄──────►│  (enroll + all job endpoints)      │
│ repo_add + files table (server side)     │        │  heartbeat / honor cancel          │
│ pacman repo server :8081 ────────────────│───────►│  pacman [repo] resolves deps       │
└──────────────────────────────────────────┘        └────────────────────────────────────┘
```

Control is **inverted**: instead of AURCache pushing to a local daemon, workers
pull jobs over HTTPS/mTLS. Concurrency now lives on the worker
(`WORKER_CONCURRENCY`).

---

## Worker protocol (`/api/worker`, mTLS)

Types live in `aurcache-common`: `JobDescriptor`, `ClaimRequest`,
`Heartbeat {active_build_ids, version}`, `CompleteReport {success, exit_code,
reason, canceled}`.

| Endpoint | Purpose |
|---|---|
| `POST /register` | self-register (name, arches, version, CSR) → `pending` (server-auth TLS, no client cert yet) |
| `GET  /register/status` | poll enrollment; returns signed cert + CA once approved |
| `GET  /ca` | unauthenticated; returns CA/server cert fingerprint for the worker to pin |
| `POST /jobs/claim` | body: arches; atomic claim; returns **JobDescriptor** or `204` |
| `GET  /jobs/{id}/source` | streams source `tar.gz` from `SnapshotStore` (patched) |
| `POST /jobs/{id}/logs` | append chunk to `builds.output` |
| `POST /jobs/{id}/artifacts` | upload built `*.pkg.tar.*` |
| `POST /jobs/{id}/complete` | `{success, error?}` → server ingests + `trigger_dependents` |
| `POST /heartbeat` | reconciliation snapshot: renews leases for the worker's reported `active_build_ids`; dropped builds are requeued |
| `GET  /jobs/{id}/status` | worker polls for **cancel** |

### JobDescriptor contents

Everything the worker needs so it never contacts the AUR directly:

- build id, pkgbase, platform/arch, build_flags
- `makepkg.conf` (default: `PKGDEST`/`MAKEFLAGS`/`PACKAGER`)
- `pacman.conf` (default: `DisableSandbox`, `SigLevel = Never`, core/extra/multilib
  mirrorlist includes, `[repo] Server=AURCACHE_PUBLIC_URL/$arch`)
- per-package config **overrides**, flagged when they differ from the base so the
  worker injects them into the chroot copy
- **mirrorlist**: arch-generic `mirrorlist_for(arch) -> Option<content>`; carried
  when present. Only x86_64 is populated today; other arches return `None` and
  the worker falls back to its image's built-in mirrorlist. Forward-ready: when
  AURCache stores a per-arch mirrorlist the same field is simply populated — no
  protocol/worker change.
- `pgp_keys`: server-resolved `validpgpkeys` (from `SnapshotStore.sourceinfo`) +
  keyserver
- job timeout, cpu/mem limits
- expected pkgnames (for server-side artifact validation)

The **server stays version-authoritative**: it parses the built version from the
uploaded package filenames during ingest (`move_and_add_pkgs` logic), so the
worker does not report a version.

---

## Identity, enrollment & security

### mTLS with an internal CA (variant B)

On first startup AURCache generates a self-signed **internal CA** (stored in the
db volume) and issues its own server cert from it. Worker identity is a client
certificate:

1. **First run:** worker generates its own keypair (`rcgen`), persists the
   private key (never leaves the host), and computes a fingerprint.
2. **Register:** `POST /register` over server-auth TLS (no client cert yet) with
   name/arches/version + CSR. Server records `pending` with the fingerprint + IP.
3. **Approve = sign the CSR** with the internal CA; the worker fetches the signed
   cert via `GET /register/status`.
4. **mTLS thereafter:** Rocket mutual TLS (`mandatory = false`) validates client
   certs against the CA; an app-level guard maps the cert serial/fingerprint to a
   `workers` row that must be `approved` (not `revoked`). Revocation is an instant
   DB check — no CRL.

The `register`/`register/status`/`ca` endpoints run on the dedicated worker
listener (`:8083`) with mutual TLS *optional* (no client cert required for
enrollment); all job endpoints on that same listener require a valid, approved
client cert. This resolves the chicken-and-egg of needing a trusted cert before
one exists. The human/tooling HTTP API on `:8080` carries no client-cert logic
at all.

**Server identity for the worker** (anti-MITM during enrollment): the worker
pins the server via `AURCACHE_SERVER_CA_FINGERPRINT` (from `GET /ca` or the
startup log), or TOFU-pins on first connect. The bundled compose auto-trusts the
internal Docker network.

### One worker command, four enrollment paths

The worker is always started the same way: it prints its fingerprint in a
prominent first-log banner and **retries registration with backoff until
approved** (never crashing while `pending`). How it gets approved varies:

1. **Default single-host — capability-based shared enrollment volume.**
   Both containers mount `/enroll` (`AURCACHE_ENROLLMENT_DIR`). The worker drops
   `<fingerprint>.csr` (public only) into it; the backend (mounts it read-only)
   auto-signs any CSR whose fingerprint is present. **No secret at all** —
   authorization is volume write access, which a network-only actor cannot get.
2. **Remote pre-approved fingerprint.** Operator puts the worker's public
   fingerprint in `AURCACHE_PREAPPROVED_WORKERS=fp[:name:arches],…`; the matching
   CSR is auto-signed. Safe to commit (public value).
3. **Interactive UI/CLI approval.** Same worker start; admin approves later; the
   worker's retry loop picks it up with no restart.
4. **Shared enrollment token** (`AURCACHE_ENROLLMENT_TOKEN`) — fallback only where
   a shared volume is not feasible; demoted due to low-entropy/leak risk.

### Upload impersonation protection

- **TLS is mandatory for non-loopback workers** (without it, token/payload could
  be sniffed/rewritten). Bundled compose stays on the internal Docker network.
- **Uploads bound to the claim:** `logs`/`artifacts`/`complete` are accepted only
  when the caller's cert maps to `builds.worker_id` **and** the build is `ACTIVE`
  with a live lease. A different approved worker → `403`.
- **Single-completion:** first valid `complete` wins; any upload after the build
  leaves `ACTIVE` is rejected (no late injection).
- **Audit + rate-limit** claims/uploads (worker id + source IP).
- Optional per-job nonce (largely redundant under mTLS identity).

### Artifact validation (sanity only)

On upload the server checks filenames against the **expected pkgnames** derived
from `SnapshotStore.sourceinfo()`, and keeps the existing `files`-table
cross-package clobber guard. This stops *wrong-named* files, **not** malicious
contents under a valid pkgname (see non-goals).

---

## Arch-aware routing

- Workers advertise `native_arches` and optional `emulated_arches`.
- `claim` matches `build.platform` against the worker's arches and **prefers
  native over emulated**: a native aarch64/armv7 worker gets its arch's jobs
  first; an x86_64 worker only picks up a foreign-arch job (emulated) if it
  advertises emulation *and* no native worker's work is pending — so foreign-arch
  jobs stay reserved for the native worker.
- Claim is a single atomic transaction: `ENQUEUED → ACTIVE`, set `worker_id` +
  `lease_expires_at`. A worker may hold multiple concurrent claims.

---

## Build liveness & failure handling

Builds run on long-lived workers, so AURCache must notice when a build stops
progressing or dies. A **per-worker heartbeat** is kept — its correctness rests on
the worker reliably detecting when any one of *its own* builds fails.

### Two failure classes

| Failure | Worker | Detected by | Outcome |
|---|---|---|---|
| Build cgroup OOM-killed (exit 137) | alive | worker sees non-zero exit → `complete{success:false}` | **FAILED** (terminal) |
| `makepkg` build error / per-build timeout (124) | alive | worker `RuntimeMaxSec`/exit → `complete{success:false}` | **FAILED** (terminal) |
| Per-build task panics, worker lives | alive | panic-safe wrapper → `complete{success:false}`; else absent from heartbeat set → reconcile | FAILED / requeue |
| Whole worker OOM / crash / power loss | dead | heartbeat stops → lease expires | **requeue** (budgeted) |
| Network partition (worker alive, unreachable) | silent | lease expires; worker self-aborts unreportable builds | **requeue** (budgeted) |

**Policy:** an *explicitly reported* failure (incl. OOM) is deterministic →
terminal `FAILED`, no auto-requeue. A *silent* worker means its in-flight builds
did not explicitly fail → re-enqueue them. A bounded `attempt_count` budget stops
a poison build (one that reliably kills its worker) from bouncing forever.

### Worker-side detection (primary signal)

Each build runs in its **own** `systemd-run --scope` (`MemoryMax` +
`RuntimeMaxSec`). Isolation means an OOM/timeout kills *only that scope* — the
worker and sibling builds survive, which is exactly why a per-worker heartbeat
suffices. The worker awaits every child's exit code: `0` → upload +
`complete{success:true}`; non-zero → `complete{success:false, exit_code, reason}`.
Each per-build task is panic-wrapped so a completion is **always** emitted — a
build is never silently dropped while the worker lives. If the server is
unreachable for `> LEASE_TTL`, the worker self-aborts those builds to avoid a
zombie upload after the server has already requeued them.

### Heartbeat = reconciliation snapshot

`POST /heartbeat {active_build_ids, version}` (identity = client cert). The server
(1) sets `workers.last_seen = now`; (2) renews `lease_expires_at = now + LEASE_TTL`
for each reported id that is `ACTIVE` and owned by this worker; (3) **reconciles**
— a build `ACTIVE`+owned but *absent* from `active_build_ids` (and not completed)
was dropped by the worker → requeue immediately (budgeted), faster than lease
expiry. This only covers the panic/lost-track edge; the explicit `complete` is the
main path.

### Lease reaper (`lease-reaper`, every `REAP_INTERVAL`)

- `ACTIVE` builds with `lease_expires_at < now` → if `attempt_count < MAX_ATTEMPTS`:
  `ACTIVE → ENQUEUED`, clear `worker_id`/lease, `attempt_count += 1`; else →
  `FAILED` ("gave up / worker lost").
- **Backstop:** fail/requeue any `ACTIVE` build older than
  `MAX_BUILD_DURATION + grace` (covers a hung build whose worker still heartbeats).
- Cancel is **excluded from requeue**: a canceled build whose worker never acks is
  force-transitioned to `FAILED`, not re-enqueued.
- Workers whose `last_seen` is stale are flagged offline (UI only).

### Parameters (defaults, env-configurable)

`WORKER_HEARTBEAT_INTERVAL` 15s · `LEASE_TTL` 60s (~4 missed beats) · `REAP_INTERVAL`
20s · `MAX_ATTEMPTS` 3 · `MAX_BUILD_DURATION` = reuse existing `JobTimeout`.

---

## Server-side changes

- **DB** (`db-schema`): new `workers` table
  `(id, name, status[pending|approved|revoked], cert_fingerprint,
  signed_cert, not_after, native_arches, emulated_arches, last_seen, version)`;
  `builds` gains `worker_id` + `lease_expires_at` + `attempt_count` (bounded
  requeue budget). `lease_expires_at` (= `now + LEASE_TTL` on each heartbeat) is
  the single liveness source of truth. The client cert replaces the bearer token
  as worker identity.
- **Repo ingest** (`refactor-repo-ingest`): extract the core of
  `move_and_add_pkgs` out of `Builder` into a reusable
  `fn(pkg_id, platform, Vec<(filename, bytes)>) -> version` that writes `./repo`,
  runs `repo_add`, and updates the `files` table. Called from the
  artifacts/complete endpoint.
- **Config generation** (`config-gen`): reuse `makepkg_utils`/`commands` from the
  API to fill the JobDescriptor.
- **Queue**: `init_build_queue` no longer spawns builders; `Action::Build`
  becomes a low-latency wakeup (optional), otherwise workers poll on an interval.
- **Lease reaper** (`lease-reaper`): scheduler job that requeues silent-worker
  builds (budgeted) and terminally fails explicit failures — see
  [Build liveness & failure handling](#build-liveness--failure-handling). Cancel
  sets a flag the worker polls via `/jobs/{id}/status`.
- **Remove Docker** (`remove-docker`): delete bollard, `docker.rs`, container
  `build.rs`, cancel-via-docker, QEMU binfmt init; delete
  `BuildMode`/`HostBuildconfig`/`DinDBuildconfig`, `get_build_mode()`, and the
  `get_repo_config()` network-detection in `build_mode.rs` — replaced by a single
  `AURCACHE_PUBLIC_URL`. Collapse the two dev compose files into one.

---

## Worker (`aurcache-worker`)

New crate + Docker image. Arch-based, with `devtools` + `base-devel` and a
non-root **build user** (passwordless sudo), run `privileged` (needs
mount/unshare for `arch-nspawn`). Persists: the **base chroot**, a per-job
**copy pool**, and the **cache volume**.

### Base chroot lifecycle (shared)

- Created once via `mkarchroot -C <pacman.conf> -M <makepkg.conf>
  $CHROOT/root base-devel`, seeded with the server default configs + mirrorlist +
  `[repo] → AURCACHE_PUBLIC_URL/$arch`.
- Kept fresh with `arch-nspawn $CHROOT/root pacman -Syu` (and `makechrootpkg -u`).

### Per-package build sequence

Up to `WORKER_CONCURRENCY` in parallel, each with a unique copy `-l job-<id>`:

1. **Fetch source** — `GET /jobs/{id}/source` → extract. Already patched
   server-side; the worker never touches the AUR or applies diffs.
2. **Fresh copy** — `makechrootpkg -c -l job-<id>` snapshots `root`.
3. **Inject per-package config** — write the job's `makepkg.conf`/`pacman.conf`
   into the copy's `/etc/` when they differ from the base; treat mirrorlist as an
   optional per-arch value (inject when present, else fall back to the image
   built-in).
4. **PGP keys** — import only `JobDescriptor.pgp_keys` into the build user's
   keyring in the copy (`arch-nspawn … gpg --recv-keys`, configurable keyserver).
5. **Build** — `makechrootpkg` → `makepkg -s <flags>` under a per-build
   `systemd-run` cgroup scope (`MemoryMax` + `RuntimeMaxSec`), so an OOM/timeout
   kills only that scope. The worker awaits the exit code and reports it (see
   [Build liveness](#build-liveness--failure-handling)).
6. **Stream logs** — combined stdout/stderr to `/jobs/{id}/logs` in chunks +
   heartbeats.
7. **Upload & complete** — POST each `*.pkg.tar.*` to `/jobs/{id}/artifacts`, then
   `POST /jobs/{id}/complete {success}`. Server parses the version and ingests.
8. **Cleanup** — drop the per-job copy + workspace; keep the base chroot.

Also provides `build-once --package <pkg>` (local chroot build, no server, for
dev iteration) and an optional `prepare` (generate identity, print fingerprint,
exit).

### Zero-config defaults

`WORKER_ARCHES = uname -m`, `WORKER_CONCURRENCY = nproc`,
`WORKER_NAME = hostname`, `AURCACHE_ENROLLMENT_DIR = /enroll`. Logs a clear
"enrolled + building" line on success.

### Worker-local caches (optional, best-effort)

Persisted under the cache volume, bind-mounted into the `makechrootpkg` copy.
Note: for `-git` packages the large upstream `source=('git+…')` clone happens
here inside makepkg, not in `SnapshotStore` — so this is the right layer to cache.

1. **Per-pkgbase `SRCDEST`** (`$CACHE/srcdest/<pkgbase>`, `SRCDEST=/srcdest`) →
   incremental `git fetch` instead of full re-clone (fixes slow
   `ttf-google-fonts-git`); reuses unchanged tarballs.
2. **Persistent `GNUPGHOME`** → import only keys not already present; no
   re-fetch/re-approve on updates (safe: PKGBUILD `validpgpkeys` still gates
   trust).
3. **pacman package cache** (`/var/cache/pacman/pkg`) reuse.
4. Optional **ccache** + `BUILDENV+=(ccache)`.

**Graceful fallback (never break a build):** missing dir → `mkdir -p` and build
cold; cache volume not mounted → ephemeral temp `SRCDEST`; corrupted/partial
clone → wipe that pkgbase subdir and retry once cold (self-heal); cache I/O
errors logged, never fatal; skip entries in use by an active build.

**Eviction:** periodic worker GC; track per-pkgbase last-used; evict LRU beyond
`WORKER_CACHE_MAX_SIZE` (~20G) and/or `WORKER_CACHE_TTL` (~30d, `0`=disable);
opportunistic GC when disk is low; same budget for the pacman cache
(`paccache`-style); ccache self-caps; keyring is tiny (no eviction); never evict
in-use entries.

**Exclusive per pkgbase, and it is the tarballs that require it.** `SRCDEST` is
keyed by pkgbase and not by platform, so an x86_64 and an aarch64 build of one
package share the directory; a plain source is downloaded as `<file>.part` and
renamed, keyed by filename, so two such builds race on the same name. Git would
tolerate the concurrency on its own -- a fetch only *adds* objects and moves
refs atomically, and makepkg's `-p` prunes refs rather than objects, so a
checkout taken at any moment stays readable. What no amount of concurrency
tolerates is *removal*: `git gc`/`prune` in a mirror, or our own eviction of it,
strands the checkouts that borrow its objects through `alternates`. Auto-gc is
live (makepkg points `GIT_CONFIG_SYSTEM` at a file that does not exist, so
`gc.auto` is at its default); `gc.pruneExpire`'s two weeks is what has kept
orphaned commits readable so far, not design.

`crate::srcdest_lock` therefore hands out one guard per pkgbase, which is both
the right to use the directory and the record that keeps the GC off it.

### Pooling the git mirrors by URL was looked at again, and parked

Two packages often share an upstream (`gtk2` and `lib32-gtk2`, four more pairs
on the reference server: 50 tracked sources over 45 distinct URLs), and each
keeps its own mirror. One bare repo per URL with a checkout per package is a
natural fit for git itself -- that is already makepkg's own two-tier shape, and
checkouts of different commits from one mirror do not interfere.

It is the *placement* that does not fit. makepkg only ever looks at
`$SRCDEST/<name>`, keyed by the source's name rather than its URL, and devtools
binds `$SRCDEST` itself, so a pool must sit at the same absolute path on the
host and inside the chroot -- the source download runs outside, the build runs
inside. That means an extra `-d /pool:/pool` bind and a symlink per source,
keyed by URL so the pool is not a namespace shared by every package on the
worker. (A name collision would at least be loud: `download_git` compares
`remote.origin.url` and fails `E_NOT_A_CLONE_OF` rather than fetching the wrong
source into an existing directory.)

Against that: locks become per source, several per build, so they must be taken
in a fixed order; and evicting one pooled mirror strands borrowed checkouts in
every package that used it rather than in one. The saving today is nil -- the
duplicated URLs are all small `#tag=` pins, and the 4.5 GB `google/fonts` mirror
has exactly one user. Parked, with the reasoning recorded so it is not
re-derived from scratch.

---

## Configuration reference

### Backend

| Env | Purpose |
|---|---|
| `AURCACHE_PUBLIC_URL` | externally reachable base URL for source + `[repo]` |
| `AURCACHE_ENROLLMENT_DIR` | shared enroll volume path (default `/enroll`) enabling capability auto-approve |
| `AURCACHE_PREAPPROVED_WORKERS` | `fp[:name:arches],…` fingerprints auto-approved at startup |
| `AURCACHE_ENROLLMENT_TOKEN` | fallback shared-secret auto-approve |
| `MIRRORLIST_SERVERS_X86_64` | existing; overrides x86_64 mirrorlist |
| `LEASE_TTL` / `REAP_INTERVAL` / `MAX_ATTEMPTS` | build-liveness reaper tuning (60s / 20s / 3) |

### Worker

| Env | Purpose |
|---|---|
| `AURCACHE_URL` | backend API base URL |
| `AURCACHE_SERVER_CA_FINGERPRINT` | pin the backend CA/server cert (or TOFU) |
| `AURCACHE_ENROLLMENT_DIR` | shared enroll volume path (bundled) |
| `AURCACHE_ENROLLMENT_TOKEN` | shared-secret enrollment (fallback) |
| `WORKER_ARCHES` | default `uname -m` |
| `WORKER_CONCURRENCY` | default `nproc` |
| `WORKER_NAME` | default hostname |
| `WORKER_CACHE_MAX_SIZE` / `WORKER_CACHE_TTL` | cache eviction budget/TTL |
| `WORKER_HEARTBEAT_INTERVAL` | heartbeat cadence (default 15s) |

---

## Deployment topologies

### A. Bundled single-host (primary, turnkey)

Ship a canonical `docker-compose.yaml` (this *is* the default config). Backend +
one privileged worker share an `enroll` volume (backend read-only, worker
read-write) on an internal network. No secret, no approval, no required env.

```
curl -O https://raw.githubusercontent.com/.../docker-compose.yaml
docker compose up -d
```

The worker self-enrolls via the shared volume and starts building; the UI shows
it connected within seconds.

### B. Remote foreign-arch worker

Two steps, one command each side:

1. Start the worker (single `docker run`); read its fingerprint from the first
   log lines. It retries until approved.
2. Add that fingerprint to `AURCACHE_PREAPPROVED_WORKERS` on the backend and
   `docker compose up -d aurcache` (or approve it in the UI/CLI).

Its native-arch jobs are prioritized to it.

---

## Frontend & CLI

Both surfaces consume the plain-HTTP worker **admin** endpoints on `:8080`
(`GET /api/workers`, `POST /api/workers/{id}/approve`, `.../revoke`) — no client
cert needed (those are only for the worker *protocol* on `:8083`).

- **CLI** (`aurcache-cli`): `worker list | approve <id> | revoke <id>`.
  `list` renders a table (id, name, status, native/emulated arches, version,
  last-seen, truncated fingerprint) or JSON with `--format json`. Backed by
  `AurCacheClient::{list_workers,approve_worker,revoke_worker}` +
  `aurcache_client::Worker`.
- **Frontend Workers page** (`lib/screens/workers_screen.dart`, route `/workers`,
  side-menu "Workers"): a live-refreshing (10s) table showing name (fingerprint
  on hover), a colored status chip (pending/approved/revoked), arches, version
  and relative last-seen, with inline **Approve**/**Revoke** buttons that toast
  and refresh. Uses a hand-written model/provider (`models/worker.dart`,
  `providers/workers.dart`, `api/workers.dart`) so it needs no `build_runner`
  codegen.

---

## Testing

Single mode everywhere — the host/dind split is gone.

1. **Rust unit/integration (no Docker, sqlite in-memory):** repo-ingest
   (synthetic pkg bytes → `repo_add` → `files`), internal-CA CSR signing +
   revocation, `/register` approval modes (volume/env/token), arch-aware claim +
   native priority, lease-reaper requeue, artifact filename validation.
2. **Protocol e2e with a fake worker (fast, no chroot/privileged):** drives the
   real HTTP protocol (enroll → claim → download → upload canned artifact →
   complete), asserting build success + repo/files update; optionally over mTLS.
3. **Full e2e (`docker-compose.e2e.yaml`):** one real privileged worker builds
   `hello` in a chroot and installs it from the repo. Mirrors the bundled compose
   and doubles as a smoke test of the shipped setup.

`scripts/test-e2e.sh` (**done**): single mode against `docker-compose.e2e.yaml`
— no `E2E_MODE`, no `dc()` mode arg, no `registry` service, no builder-image
build/push, no `docker.sock`. It builds the server + worker images, brings the
stack up, waits for the API (plain HTTP `:8080`) and for a worker to enroll and
auto-approve over mTLS (`:8083`) — `wait_for_worker` polls `/api/workers` until
one reports `approved` — then requests the package, waits for the build, and
installs it from the repo (`:8081`) in a throwaway container. Named volumes make
teardown a plain `docker compose down -v`. CI keeps `./scripts/test-e2e.sh
hello`. The obsolete `test-builder` binary/`test-builder.sh` are removed in
favour of `aurcache-worker build-once`.

---

## Implementation phases (critical path)

1. `db-schema`, `worker-protocol` — foundation.
2. `refactor-repo-ingest`, `config-gen` — decouple from the Docker builder.
3. `internal-ca`, `api-worker-endpoints` — mTLS/enrollment core.
4. `arch-routing`, `worker-approval`, `enroll-modes`, `upload-binding`,
   `artifact-safety`, `server-trust` — claim + security.
5. `worker-crate` — the worker binary (devtools build loop) + `build-liveness`
   (heartbeat + per-build OOM/timeout detection) + `worker-cache`,
   `worker-oneshot`.
6. `lease-reaper`, `reachability`, `bundled-turnkey`.
7. `remove-docker` — tear out the old path once the new one is proven.
8. `frontend-cli`, `docs`, `unit-tests`, `fake-worker-test`, `e2e-update`,
   `non-goals`.
