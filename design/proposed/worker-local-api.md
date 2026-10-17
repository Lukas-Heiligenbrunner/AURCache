# A local control API on the worker

Status: **Proposed** · Last updated: 2026-09-28

A socket on each worker that whoever runs that machine can use to see what it
is doing and steer it: active builds, storage, settings, stopping a build,
pausing intake, and running a build of their own. It is not a second channel
for the server. The server keeps talking to the worker the way it does now,
over the heartbeat; this is for the person with a shell on the box.

---

## Where things stand

Everything an operator can learn about or do to a worker goes through the
server:

- **What it is doing**: the Workers page and `aurcache-cli worker` show what the
  server knows -- the builds it leased to the worker, the effective
  configuration the heartbeat last reported, when it was last heard from.
- **Changing it**: settings are saved on the server and delivered over the
  heartbeat (`design/implemented/worker-configuration.md`); pausing is a flag
  on the server's `workers` row; stopping a build is the server putting its id
  in `HeartbeatResponse::cancel`.

On the machine itself there is the journal, `/etc/aurcache/worker.env` and a
restart. That leaves gaps that show up in exactly the situations where someone
*is* on the machine:

- **When the server is not reachable**, or the worker is not approved yet,
  there is nothing to ask. "Why is this worker not building?" is answered by
  reading the journal. The server's `WaitingReason` covers the server's half of
  that question; the worker's half -- the pool would not open, a base chroot
  refresh is draining, the disk floor is not met, the gate is full -- is only
  in the log.
- **Storage is invisible.** The worker knows the pool's quota and usage
  (`Pool::total_usage`, `host_free`), what each running build holds
  (`Chroots::build_usage`), the cache budgets and which failed builds' trees it
  is keeping, and until when (`KeptBuild`). None of it is reported anywhere but
  the log, and a kept tree's path is what someone needs to `arch-nspawn` into
  it.
- **The machine's owner is not always the server's operator.** A worker lent to
  a shared AURCache by someone who does not administer that server can only
  stop contributing by stopping the service, which kills whatever is running.
- **`build-once` runs beside the daemon, not through it.** It opens its own
  `Chroots` on the same pool, with its own view of the caches, outside the
  concurrency gate. It was written for a machine with no daemon running.

---

## Who it is for

The **local operator**: root on the worker host, or someone given the same
access to the worker as its service account. They can already stop the
service, edit its environment and read its data directory. The API gives them
nothing they could not get to with a shell; it makes it safe to do while builds
run, and makes it one command instead of a procedure.

It is **not** for:

- **The server.** It has its protocol, authenticated by the worker's
  certificate, and a server that could also reach into each worker would need
  the worker to be reachable from it, which the pull-only design deliberately
  avoids (`design/implemented/remote-workers.md`).
- **Builds.** A PKGBUILD runs on the same host as the worker, in the same
  network namespace. Whatever this API can do, a build must not be able to
  reach it. That rules the transport (below).
- **Remote dashboards**, at least in the first cut. See [Metrics](#metrics).

---

## 1. Transport: a Unix socket, never a TCP port

### Why not TCP on localhost

Builds share the host's network namespace -- systemd-nspawn runs without
`--network-*` -- which is why `build_firewall` exists: a crafted PKGBUILD can
dial anything the host can, `127.0.0.1` included. A control port on localhost
would be reachable from every build. Guarding it with the same `(uid, port)`
rule as the worker-protocol port would work when the rule installs, but that
rule is best-effort by design (`WORKER_BUILD_FIREWALL=0`, hosts without
iptables), and a failure there would expose "cancel any build, change any
setting, run a build" rather than "submit a registration that still needs
approving". A bearer token on top would then be a secret on disk the build user
must not be able to read -- which is the same filesystem permission a socket
gets for free.

### The socket

`$RUNTIME_DIRECTORY/control.sock`, i.e. `/run/aurcache-worker/control.sock`
for the native package (`RuntimeDirectory=aurcache-worker` in the unit),
overridable with `WORKER_CONTROL_SOCKET` and disabled with
`WORKER_CONTROL_SOCKET=off`.

- **A path-bound socket, never an abstract one.** Abstract Unix sockets are
  scoped to the network namespace, not the filesystem, so a build could reach
  one.
- **Mode `0660`, owner `aurcache:aurcache`.** Only the service account and
  members of its group can connect. Not the `aurbuild` group: the worker shares
  that one with the build user for the caches.
- **Checked again on accept** with `SO_PEERCRED`: the peer's uid must be 0, the
  worker's own, or a member of the socket's group. A defence against a socket
  path or mode someone loosened, not the primary control.
- **Not inside any chroot.** `/run` is not bound into builds. The one way to
  undo that is `WORKER_BIND_MOUNTS`; the worker refuses to start the listener if
  a configured bind mount contains the socket's directory, and says why.

### Containers

The container images (hybrid, `aurcache-worker-docker`) put the socket in the
container's `/run`. `docker exec <worker> aurcache-worker status` is the local
path there; mounting the directory out is possible and the operator's choice.

### Remote use

Out of scope, and mostly unnecessary: `ssh host aurcache-worker status`, or
`ssh -L /tmp/w.sock:/run/aurcache-worker/control.sock host` to point a local
client at it.

---

## 2. Protocol: HTTP/1.1 and JSON over the socket

- **HTTP**, so `curl --unix-socket /run/aurcache-worker/control.sock
  http://worker/v1/status` works for anyone scripting it, and log streaming is a
  chunked response rather than a framing to invent.
- **hyper 1.x with `hyper-util`**, both already in the lockfile through
  reqwest. A dozen routes do not need a framework, and the worker binary stays
  free of Rocket.
- **Types in `aurcache-common`** under `worker::local`, next to the worker
  protocol, so the client, the worker and anything else share them rather than
  re-declaring them.
- **Versioned path prefix** (`/v1/`). The client ships in the same binary as the
  daemon, so a mismatch only happens across an upgrade that has not restarted
  the service yet; the version lets the client say that instead of failing on
  a shape.

### The client

The worker binary itself, as subcommands beside `run`, `prepare` and
`build-once`:

```text
aurcache-worker status                  # identity, server, intake, gate, storage summary
aurcache-worker builds                  # running builds with usage
aurcache-worker logs <build>            # follow a running build's log
aurcache-worker stop <build> [--requeue]
aurcache-worker pause [--wait] | resume
aurcache-worker config [--set k=v] [--reset k]
aurcache-worker storage                 # pool, caches, kept trees
aurcache-worker kept [discard <build>]
aurcache-worker build <dir> [--flag ..] # out-of-band build through the daemon
```

`--format json` as `aurcache-cli` has it. The protocol client lives in
`aurcache-worker-core` so the docker worker's binary gets the same commands.
Not in `aurcache-cli`: that is the server's client, installed on machines that
are not workers, and it should not grow commands that only work on the host
they run on.

---

## 3. What it exposes

Split by where it lives, because that decides what the legacy container worker
gets for free:

- **Runner-level** (`aurcache-worker-core`): identity, server contact, the gate,
  the active-build map, settings, pause. Every executor has these.
- **Executor-level**: storage, kept trees, out-of-band builds. Through new
  `Executor` methods with defaults that say "not supported", like
  `reconfigure`.

### Status (read-only)

`GET /v1/status`:

| Field | Source |
|---|---|
| name, fingerprint, kind, version | `CoreConfig`, `Identity`, `Executor::KIND` |
| server URL, enrollment state (pending / approved / revoked), last contact | enrollment, `Runner::last_contact` |
| intake: claiming / paused locally / paused by the server / not ready (and why) | see below |
| gate: target, running | `ConcurrencyGate` |
| active builds: count | `Runner::active` |
| storage summary: pool used / quota, host free | executor |
| settings: count refused, count overridden locally | `EffectiveConfig` |

**Why it is not claiming** is the one field worth designing for: it is the
worker's half of `WaitingReason`. `Executor::ready_for_work` answers a bool
today; it becomes `readiness() -> Readiness` with a reason (`PoolUnavailable
{ since, error }`, `RefreshDraining`, `DiskFloor { need, free }`,
`BuildLimit { .. }`), and `ready_for_work` stays as `readiness().is_ready()`.
The runner adds its own: gate full, paused locally, server unreachable, not yet
approved. The server does not know whether it paused a worker unless told, so
"paused by the server" is only shown if the heartbeat response carries it --
which is cheap to add and worth it, since a paused worker looks idle from the
machine.

### Builds

`GET /v1/builds`: per running build, its id, package, arch, when it was
claimed, its cgroup's current memory and CPU, and its disk usage
(`Chroots::build_usage`). The server knows the first four; the last three only
the worker knows while the build runs, and they are what someone looking at a
loaded machine wants.

`GET /v1/builds/<id>/log?follow`: the build's output as it is produced. The
executor already sends it to the server in batches; a local tail is a
`broadcast` of the same lines, dropped when nobody listens. Useful when the
server is unreachable or the log is being capped there
(`build-log-capping.md`).

### Stopping a build

`POST /v1/builds/<id>/stop` sets the build's cancel flag, the same one a
server Stop sets. What the server then records is the question, because today
it only expects a cancelled report for an abort *it* started
(`aurcache-api/src/worker.rs`, the `acknowledged_abort` branch); an unsolicited
one is a plain failure. Two intents:

- **Stop**: the build ends here, recorded as stopped on the worker. The
  `CompleteReport` gains `stopped_by: Option<StopOrigin>` (`Server`,
  `LocalOperator`), and the server records an end reason and an activity entry
  naming the worker, the way an operator Stop names the user.
- **`--requeue`**: give the job back, so it runs elsewhere or later. The worker
  aborts and reports it as abandoned; the server requeues it as it does after a
  lost lease, without waiting for the lease to expire. On its own it would
  often come straight back to the same worker, so `--requeue` is most useful
  with `pause`, and the client says so.

### Pause and resume intake

`POST /v1/pause`, `POST /v1/resume`. Local and server pause are separate
flags; either stops claims. The local one is enforced in the runner's claim
loop, before the gate.

- **`--wait`** returns when no build is running, for "pause, wait, then
  `systemctl stop`" before maintenance.
- **Persisted** in the data directory, so a machine paused for maintenance does
  not start claiming the moment it boots. `status` says it is paused and since
  when, and so does the log at startup.
- **Reported to the server** in the heartbeat. Without that the server keeps
  counting the worker as available for priority hold-back, and lower-priority
  workers wait on it for `WORKER_SPILL_DELAY`. With it, the scheduler treats it
  like a server pause and `WaitingReason::Paused` can say which side paused it.

### Settings

`GET /v1/config` returns the declaration and the effective configuration --
what the Workers page shows, readable when the server is not.

`PATCH /v1/config` with `{settings: {key: value | null}}`, as the server's
endpoint takes, validated against the same declaration with the same
`ValueKind` parsers, applied as one set.

The question is where a local value sits in the precedence the worker already
resolves:

1. the pin (`WORKER_CONCURRENCY`),
2. the server's value,
3. the environment default (`WORKER_CONCURRENCY_DEFAULT`),
4. the built-in default.

Options:

- **A. Above the server, below the pin.** A local value is a runtime pin: it
  beats the server, and the server sees it reported with a new source, `local`,
  and its own value as `overridden`. Resetting it hands the key back to the
  server. The env pin still wins and the local API refuses to set a pinned key,
  naming the variable, as the server's UI does.
- **B. Below the server.** A local value only matters until the server sets
  one. It gives the machine's owner no control they did not have by editing
  `_DEFAULT` and restarting, which is the thing this is meant to avoid.
- **C. Forwarded to the server.** The worker saves the value on the server,
  under its own certificate, and it arrives back like any other delivery. One
  place values live, and the Workers page stays the whole story. But it needs
  the server reachable, a new endpoint that lets a worker write its own
  settings, and a machine owner who is not the server's operator ends up
  editing someone else's configuration.

**Recommended: A.** It is what "local control" means, it keeps working with the
server down, and the reporting that already exists makes it visible rather than
a silent divergence. It needs:

- `Source::Local` in `aurcache-common`, and the UI rendering it the way it
  renders an env pin: the server's value stored, shown as not in effect, with
  "overridden locally on this worker".
- A local layer in `WorkerSettings` beside `with_snapshot`, persisted as
  `local-settings.json` in the data directory and loaded at startup, so a
  restart does not quietly hand a key back to the server.
- The same apply path as a delivery: resolve, `Executor::reconfigure`, gate,
  swap, `sync_registration`. So a local concurrency change re-registers, and
  `applies` means the same thing whichever side changed a value.

Only declared keys. The local operator could edit the environment instead, and
the undeclared keys (paths, the build user, bind mounts) all need a restart,
which is what the environment is for.

### Storage and kept trees

`GET /v1/storage`: the pool's quota, usage and whether it is oversized; host
free space; per cache (sources, packages, build trees) its size, budget and
TTL; per running build its share. Most of it is already computed for
admission and eviction.

`GET /v1/kept`: each failed build whose chroot is kept, with its paths
(`KeptBuild::chroot`, `data`), its size and when it expires, so the answer to
"let me look at why it failed" is a path. `DELETE /v1/kept/<id>` discards one
early.

Maintenance, as actions rather than settings: `POST /v1/storage/evict` (run
the cache eviction now), `POST /v1/chroots/refresh` (refresh the base chroot
now rather than at the next interval).

### Out-of-band builds

`POST /v1/builds` with a PKGBUILD directory runs a build that the server never
hears of, through the daemon:

- **Through the gate**: it takes a slot, so it does not overcommit the machine;
  the claim loop simply takes less. `status` counts it separately.
- **Through the same pool and caches** as served builds, with the served
  build's disk quota and limits.
- **The directory is sent, not named.** The daemon runs as `aurcache` and
  cannot be assumed to read the caller's home. The client tars the directory
  and streams it; the daemon unpacks it into the build's tree. The client
  reading it as the caller is also the permission check.
- **Log streamed back** on the same request, and the artifacts downloadable
  from `GET /v1/builds/<id>/artifacts` until the build's tree is released,
  written by the client into the caller's directory -- which is where
  `build-once` puts them today.
- **Pacman configuration**: the server's repository and mirrorlist, as a
  served build would get -- "build it the way this worker builds" -- with a
  `--host-pacman-conf` flag for `build-once`'s behaviour.
- **Ids from a local range**, disjoint from the server's, like
  `ONE_SHOT_BUILD_ID`, so the pool's labels and sweep never mistake one for a
  served build. Not persisted: a restart sweeps them.

What it is not: a way to publish. Artifacts never reach the repository. A
build meant for the repository is an ordinary build on the server, which can
already be pinned to a worker by package affinity.

`build-once` stays as it is, for a machine with no daemon running. With one
running, the client should use the socket, and `build-once` should refuse
while the socket answers -- to be checked: whether two `Chroots` opening one
pool can sweep each other's leases.

---

## 4. What the server sees

Nothing about the local API itself. What changes on the worker reaches the
server through what already reports it:

| Local action | Server learns it through |
|---|---|
| Setting changed | the effective configuration in the heartbeat, `source: local` |
| Paused / resumed | a heartbeat flag |
| Build stopped | `CompleteReport::stopped_by` |
| Build requeued | an abandonment report |
| Out-of-band build | a count in the heartbeat, for the Workers page; nothing else |

Each of those server-side changes is optional in the protocol, `#[serde(default)]`
both ways, as the configuration fields were: an older server ignores them and
an older worker never sends them.

---

## 5. The legacy container worker

It shares `aurcache-worker-core`, so the runner-level half -- status, builds,
stop, pause, settings -- comes with no work. Storage, kept trees and
out-of-band builds are executor methods it does not implement, and the API
answers `501` with what the executor is. It is not worth building them for an
executor that is on its way out.

---

## Metrics

A Prometheus scrape needs TCP, and this design refuses TCP. If it is wanted,
it is a separate listener serving only `GET /metrics` -- counters and gauges,
no control -- opt-in with `WORKER_METRICS_ADDR`, documented as reachable by
builds. What a build could learn from it is what else this machine is building,
which is little but not nothing. Better still: the server already has the
fleet's builds, and `home-assistant-integration.md`'s metrics surface is the
place for fleet-wide numbers. Not in this design.

---

## Recommendation

1. **Read-only**: the socket, the client, `status`, `builds`, `storage`,
   `kept`, `config` (read). `readiness()` with reasons. Useful on its own and
   with no protocol change.
2. **Control that needs no server change**: `pause`/`resume` (local only),
   `stop` recorded as today's plain failure, `kept discard`, `storage evict`,
   `chroots refresh`, log follow.
3. **Server-visible control**: the heartbeat's pause flag and the scheduler
   honouring it; `stopped_by` and `--requeue`; `Source::Local` and the local
   settings layer.
4. **Out-of-band builds** through the daemon, and `build-once` deferring to a
   running daemon.

## Open questions

- **Who may connect.** The service account's group is the obvious one, but on a
  native install `aurcache` is also the group of files builds must not write.
  A dedicated `aurcache-admin` group is cleaner and one more thing to set up.
- **Local pause persisting across restarts.** Right for maintenance; surprising
  if someone paused, forgot, and restarted the service expecting it to build.
  The startup log line may be enough.
- **Local settings vs an env pin set later.** If an operator sets
  `build_timeout` locally and later adds `WORKER_BUILD_TIMEOUT` to the
  environment, the pin wins and the local value is stored but shadowed -- the
  same rule as a server value. Worth confirming that is what is wanted rather
  than refusing to start.
- **`--requeue` landing on the same worker.** Should a requeue from a worker
  exclude that worker for the build's next claim, rather than relying on the
  operator pausing it?
- **Out-of-band builds from a package name** (`aurcache-worker build --aur
  foo`), fetching the AUR snapshot locally: convenient, but it is the server's
  source resolution done a second way. Left out until asked for.
