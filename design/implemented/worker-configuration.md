# Configuring workers from the server

How a worker's settings could be seen and changed through AURCache's API and UI,
instead of only through each machine's environment, and how the change would
reach a running worker.

Status: **implemented** (phase 1, steps 1-6). The worker declares its settings,
reads `<env_var>_DEFAULT` for each and reports what each resolved to; values are
set per worker on its page or with `aurcache-cli worker config`, saved in one
transaction, delivered over the heartbeat and applied without a restart. Two
deliberate divergences from the text below are recorded where they apply: the
snapshot is not returned at registration ([Recommended](#compared)), and the
concurrency gate is a target and a count rather than a semaphore with a deficit
([Concurrency](#concurrency)). [Drain](#drain) is built as described, named pause and resume, and the legacy
container worker declares its own settings (see [Recommendation](#recommendation)).
Not built: fleet defaults, and phase 2 (SSE).

- The first draft had a server-side allowlist of worker settings and a fleet
  default. Two reviews ([`worker-configuration-review.md`](../reviews/worker-configuration-review.md),
  [`worker-configuration-review-codex.md`](../reviews/worker-configuration-review-codex.md))
  agreed with the security boundary and the transport and found the protocol
  under-specified; the first revision adopted most of what they raised.
- This revision replaces the allowlist with **settings each worker declares**,
  drops fleet defaults for now, and lets the worker **re-register** when a change
  alters what it registered with. The server no longer knows what any worker
  setting means.
- The third lets the environment set a **default the server may override**
  (`WORKER_CONCURRENCY_DEFAULT`) beside the existing pin (`WORKER_CONCURRENCY`).
  [History](#history) records what each round changed.

---

## Where things stand

A worker is configured entirely by its environment: `/etc/aurcache/worker.env`
on a native install, the container's environment in the images. About 45
variables, parsed at startup (`aurcache_worker_core::config`,
`aurcache_worker::config`). Changing one means a shell on that machine, an edit,
and a restart -- which kills any build in progress, because builds live in the
worker's cgroup.

The server sees part of it. Registration, which a worker performs once per
process start, carries its name, architectures, concurrency, priority and
package affinity (`RegisterRequest`), and the Workers page shows those.
Everything else -- limits, cache budgets, timeouts, intervals -- is invisible to
the server.

The server can reach a worker only by answering it. The worker is pull-only:
it claims jobs, posts logs and posts a heartbeat every `WORKER_HEARTBEAT_INTERVAL`
(15 s), and `HeartbeatResponse { cancel }` is the one channel back. A Stop takes
up to a heartbeat interval to arrive, plus the build loop's 5 s tick.

### What that has cost recently

- Raising `unreal-engine`'s build-tree budget to `WORKER_BUILDDIR_MAX_BYTES=450G`
  would have been silently ignored: the variable took bytes only, and a value
  that did not parse fell back to the 200 GiB default. It was caught by reading
  the parser, not by anything the worker showed. Parsing failures now warn, but
  a warning in a worker's journal is still a place nobody looks.
- CPU and memory limits existed as *server* settings, `cpu_limit` and
  `memory_limit`, that nothing read after builds moved to workers. They have
  been removed in favour of `WORKER_BUILD_MEMORY_MAX`/`WORKER_BUILD_CPUS`.
  `max_concurrent_builds` and `builder_image` are dead the same way and are
  still on the settings API.
- Giving `unreal-engine` a bigger build-tree budget meant an SSH session to the
  one worker allowed to build it.

The last two point at the same thing: a setting that belongs to the worker has
nowhere on the server to live, so it was either put in the wrong place or left
unreachable.

---

## 1. The worker declares its settings

A worker tells the server which settings it accepts, at registration. The server
stores that declaration and renders, validates and delivers values for it
generically. It does not know what `concurrency` or `build_memory_max` *mean*,
only their types.

### Why the worker and not the server

- **The server and the workers are deployed separately** -- the server as a
  container on one host, a worker as a native package on another, upgraded when
  each is. With a server-side list, a new worker setting needs a server release
  before anyone can set it. Declared, it appears when the worker that has it
  registers.
- **Different worker implementations have different settings.** The legacy
  container worker (`aurcache-worker-docker`) reads `CPU_LIMIT` in milli-CPUs
  and `MEMORY_LIMIT` in MB; it can declare exactly those, with its own
  descriptions, instead of the server translating a shared vocabulary onto it.
- **The boundary is enforced where it is declared.** Only the worker ever
  decided what it would accept; now the same code says so.

### The declaration

Sent in `RegisterRequest`, one entry per setting:

```text
SettingDecl {
  key:         String,          // "build_memory_max"
  kind:        ValueKind,       // see below
  description: String,
  category:    String,          // "Build limits", "Caches", ...
  default:     Option<String>,  // the fallback in effect: `<env_var>_DEFAULT` if set, else built in
  env_var:     Option<String>,  // "WORKER_BUILD_MEMORY_MAX": pins it; `_DEFAULT` suffixed, a default
  applies:     NextJob | NextLoop | Immediately,
}
```

The one thing server and worker must share is **`ValueKind`**, a closed list
in `aurcache-common` with its parsers:

| Kind | Parsed with | Rendered as |
|---|---|---|
| `Integer { min, max }` | integer, range-checked | number input |
| `Float { min, max }` | float, range-checked | number input |
| `Bool` | `true`/`false` | toggle |
| `Size` | `units::parse_size` | text, shown with `units::format_size` |
| `Duration` | `units::parse_duration` | text |
| `Choice { options }` | one of the options | select |
| `Text` | as is | text |
| `List` | comma-separated | text |

A declaration with a kind the server does not know (a newer worker) is stored
and shown as text; the worker still validates it on delivery, where a bad value
becomes a `rejected` status rather than a failure.

### What a worker must not declare

The worker runs devtools as root (`aurcache ALL=(ALL) NOPASSWD: ALL`, with the
reasoning in `packaging/aurcache-worker.sudoers`), and the isolation that
constrains untrusted input is the chroot and the separate build user. Today a
compromised server can make a worker build anything, but that code runs as
`builder` inside a chroot. A server that could set `WORKER_BIND_MOUNTS` could
bind `/` into the chroot; one that could set `WORKER_BUILD_USER` or
`WORKER_MAKECHROOTPKG` could run a build as root outright.

So the declaration is **compiled into the worker**. It is never derived from the
environment ("expose every `WORKER_*` variable") or from anything the server
sends, and a value for a key the worker did not declare is refused however it
arrives. What the native worker declares:

| Class | Keys | Declared? |
|---|---|---|
| **Bootstrap** | `AURCACHE_URL`, `AURCACHE_SERVER_CA_FINGERPRINT`, `AURCACHE_ENROLLMENT_TOKEN`, `AURCACHE_ENROLLMENT_DIR`, `WORKER_DATA_DIR` | **No.** Needed before the server can be reached. |
| **Local network view** | `AURCACHE_REPO_HOST`, `AURCACHE_REPO_URL` | **No.** How this machine reaches the repository (split DNS, a VPN, a proxy). |
| **Protocol timing** | `WORKER_HEARTBEAT_INTERVAL` | **No.** Has to stay well inside the server's `LEASE_TTL` and `WORKER_LIVENESS_TIMEOUT` (both 60 s). |
| **Host access** | `WORKER_CHROOT_DIR`, `WORKER_CACHE_DIR`, `WORKER_BUILD_USER`, `WORKER_BIND_MOUNTS`, `WORKER_MAKECHROOTPKG`, `WORKER_GIT_SSH_KEY`, `WORKER_SSH_KNOWN_HOSTS`, `AURCACHE_NSPAWN_KEEP_UNIT`, `AURCACHE_DROPIN` | **No.** The security boundary above. |
| **Machine facts** | `WORKER_ARCHES`, `WORKER_EMULATED_ARCHES`, `WORKER_NAME`, `WORKER_CHROOT_OVERLAY` | **No.** Reported, not chosen. |
| **Mirrorlist** | `WORKER_MIRRORLIST_SERVERS`/`_FILE` | **No.** The server already delivers a per-architecture mirrorlist at registration, and these override it locally (`design/implemented/mirrorlist-configuration.md`). |
| **Policy and tuning** | `WORKER_CONCURRENCY`, `WORKER_PRIORITY`, `WORKER_PACKAGES`, `WORKER_BUILD_MEMORY_MAX`/`_SWAP_MAX`/`_CPUS`, `WORKER_TOTAL_BUILD_MEMORY_MAX`/`_SWAP_MAX`/`_CPUS`, `WORKER_BUILD_TIMEOUT`, `WORKER_BUILDDIR_MAX_BYTES`/`_MIN_FREE`, `WORKER_CACHE_MAX_SIZE`/`_TTL`, `WORKER_PKGCACHE_MAX_SIZE`/`_TTL`, `WORKER_SRCCACHE_MAX_SIZE`, `WORKER_CHROOT_REFRESH_INTERVAL`, `WORKER_POLL_INTERVAL`, `WORKER_KEYSERVER` | **Yes.** |

`WORKER_KEYSERVER` is safe to declare: signature checks are pinned by the
PKGBUILD's `validpgpkeys`, so a keyserver can withhold a key but not substitute
one.

### Per worker only, for now

Values are set for one worker at a time. There is no fleet default.

A fleet default is the only feature that would need workers to agree on what a
key means: one value applied to every worker that declares `build_timeout`
assumes they all mean the same thing by it, with the same kind -- and some keys
must never have one (a fleet-wide `packages` would give every worker the same
reservation, the opposite of affinity). Per-worker values need none of that.
If fleet defaults are wanted later, they can be added on top: a default applies
to the workers whose declaration has that key and kind, a declaration can mark a
key as having no fleet default, and a key declared with conflicting kinds is
refused at fleet scope.

### Precedence

Each declared setting reads two environment variables, which say different
things:

| In the worker's environment | Meaning |
|---|---|
| `WORKER_CONCURRENCY=4` | **Pin**: wins over the server |
| `WORKER_CONCURRENCY_DEFAULT=4` | **Default**: used unless the server sets a value |

Resolution per key on the worker, highest first:

1. the pin (the declared `env_var`),
2. the value set for this worker on the server,
3. the environment default (`<env_var>_DEFAULT`),
4. the built-in default.

**A plain variable pins**, as it does today: it is the model the server's own
settings use (`Package -> Env -> Global -> Default`), it is how the mirrorlist
override works, and deploying this changes nothing for an existing worker. A
machine can still veto a value for itself -- its memory limit, say -- without
the server being able to change it.

**`_DEFAULT` is what makes a value manageable without giving up the machine's
own starting point.** Without it, a setting could only be handed to the server by
deleting its variable, which dropped the machine back to the built-in default
until someone set a server value. Why a second name rather than the alternatives:

- **Versus a list of server-managed keys** (`WORKER_SERVER_MANAGED=concurrency,...`):
  one line says both the value and whether the server may override it; with a
  list, reading `WORKER_CONCURRENCY=4` would not say whether it is a pin.
- **Versus a global switch** turning every variable into a default: per setting,
  so a machine can pin its limits and leave its concurrency to the server.
- **Versus a value syntax** (`WORKER_CONCURRENCY=default:4`): no new syntax, and
  every existing parser stays as it is.

Details:

- **Both set**: the pin wins; the worker warns and reports it, since it is almost
  certainly a mistake.
- **The UI shows the real fallback**: the declaration's `default` is the
  environment default when there is one, and the effective source says
  `env_default`, so resetting a server value shows the value the worker returns
  to, not a built-in one.
- **Useful before server values exist**: with no server value, `_DEFAULT` behaves
  exactly like the plain variable, so it can ship with the read-only phase.

### Adopting it on existing workers

Most deployed workers start with most rows pinned, because their policy is
already in plain variables. A pin is a deliberate local veto, so that is right,
but moving a value to the server should be easy:

- **Adoption is a rename**: `WORKER_CONCURRENCY=2` becomes
  `WORKER_CONCURRENCY_DEFAULT=2`, and the machine keeps its value until the server
  sets one. Bootstrap, network and host-access variables stay as they are.
- **The UI names the pin and the way out**: "Pinned by `WORKER_CONCURRENCY` on
  this worker; rename it to `WORKER_CONCURRENCY_DEFAULT` to manage it here." A
  value saved for a pinned key is stored and shown as *not in effect*, never as a
  change that took.
- **Shipped files write defaults, not pins.** `docker-compose.remote-worker.yaml`
  sets `WORKER_CONCURRENCY=2`, and `aurcache-cli setup compose`/`setup worker`
  write `WORKER_CONCURRENCY` when given; they switch to the `_DEFAULT` names.
  Only in files for workers new enough to read them: an older worker ignores
  `_DEFAULT` and silently runs its built-in default.

### Storage

- **The declaration** as JSON on the `workers` row, replaced at each
  registration, so a worker's settings can be viewed and edited while it is
  offline.
- **Values** in `worker_settings (worker_id NOT NULL, key, value)`, with
  `UNIQUE (worker_id, key)` and `worker_id` referencing `workers(id) ON DELETE
  CASCADE`. Without a fleet default there is no `NULL` scope and no partial
  index.
- **The last reported effective configuration** as JSON on the `workers` row.
- **Dump and restore** carry `worker_settings`, remapped by certificate
  fingerprint the way `restore.rs` already matches workers, and dropped with a
  worker whose fingerprint is not restored.

A stored value whose key the worker no longer declares (renamed or removed in a
newer version) is kept and shown as "no longer offered by this worker", never
deleted silently.

### Validation

The server validates a value against the worker's stored declaration when it is
saved, using the `ValueKind` parsers, so the UI rejects `1.5G` for a `Size` at
save time.

The worker can still refuse a value the server accepted: a key it no longer
declares, a CPU limit on a host whose cgroup `cpu` controller cannot be enabled,
a memory limit above what the machine has. Then:

- a refused value **keeps the worker's previous usable value** for that key --
  the last one it applied, or its environment/default if it never had one. A
  limit is never loosened by a rejection;
- the worker reports **per-key status**: `applied`, `overridden` (env-pinned),
  `unsupported` (not declared), or `rejected` with a short, operator-readable
  reason;
- the UI flags the worker, rather than showing the saved value as in effect.

### Saving is one transaction

An operator often changes related keys together -- lower concurrency with a
higher per-build memory limit, say. Written row by row, a heartbeat between two
writes could deliver a combination nobody chose. So a save is one `PATCH` of a
set of keys for one worker: validate every key first, write the rows in one
transaction, record one activity entry (`WorkerConfigUpdated { worker,
changed_keys }`). The snapshot is only ever read from committed rows, so a
worker cannot see half a save.

### Visibility is worth having on its own

Independent of editing: the worker reports its **effective** configuration, with
the source and status of each value, and the Workers page shows every declared
setting and where its value came from. That alone would have shown a `450G` that
meant 200 GiB. It is the first thing to ship.

---

## 2. How a change reaches the worker

What actually needs to be timely:

| Message | Direction | Latency that matters |
|---|---|---|
| Stop a build | server -> worker | seconds (an operator is watching) |
| Configuration changed | server -> worker | tens of seconds is fine |
| Drain / resume | server only | immediate, and needs no message (see Drain) |
| Leases, liveness | worker -> server | the heartbeat interval, by design |
| Logs | worker -> server | already streamed by POST |

Only Stop is latency-sensitive, and it already works through the heartbeat.

### Options

**A. Registration only.** The server returns the worker's configuration in the
registration response; a change applies at the next start. Smallest change, no
new transport. But "apply" means "restart when idle", which on a worker building
`unreal-engine` is hours away.

**B. Heartbeat piggyback.** The heartbeat carries the revision of the last
snapshot the worker received; when the server's differs, `HeartbeatResponse`
includes the new snapshot. Latency is one heartbeat interval. No new endpoint,
no new connection, nothing new for a proxy to pass; it is exactly how cancel
already travels, with the same mTLS identity and the same failure behaviour --
if the server is unreachable, nothing changes.

**C. Long-poll.** `GET /worker/events?since=<cursor>` held open until there is
something to say, or for 30-60 s. Near-instant, plain HTTP, and proxies tolerate
it. But it is a second connection per worker, and a cursor to keep consistent
across server restarts.

**D. Server-sent events.** `GET /worker/events` as an event stream: server to
worker only, worker to server stays on POST. Rocket has it built in
(`rocket::response::stream::EventStream`, no new dependency), and reqwest reads
a streaming body, so the worker needs no new dependency either. Reconnects are
the client's job, and the stream is one-way, which is all that is missing today.

**E. WebSocket.** One persistent, bidirectional, mTLS connection per worker;
heartbeat, cancel, configuration and possibly logs all move onto it. Instant in
both directions. New dependencies on both sides (`rocket_ws`,
`tokio-tungstenite` with a rustls client certificate), and a new failure model:
half-open connections need pings, reconnects need backoff, messages need
versioning, and a reverse proxy must pass the upgrade. The server has to track
which process holds which worker's socket, which a single server does in memory
and several replicas cannot.

**F. gRPC bidirectional streaming** (tonic). Everything E offers with generated
types. A second RPC stack beside Rocket and a new protocol for anyone
debugging a worker; far more than this needs.

### Compared

| | A. Registration | B. Heartbeat | C. Long-poll | D. SSE | E. WebSocket | F. gRPC |
|---|---|---|---|---|---|---|
| Config latency | next restart | ≤ 15 s | ~instant | ~instant | ~instant | ~instant |
| Stop latency | as today | as today | ~instant | ~instant | ~instant | ~instant |
| New dependencies | none | none | none | none | 2 | several |
| New endpoints | 0 | 0 | 1 | 1 | 1 (upgrade) | a service |
| Connections per worker | as today | as today | +1 held | +1 held | 1, persistent | 1, persistent |
| Proxy concerns | none | none | idle timeout | idle timeout, buffering | upgrade passthrough | HTTP/2 end to end |
| If the channel drops | n/a | nothing changes | reconnect | reconnect | reconnect; leases still need a fallback | as E |
| Server replicas | fine | fine | fine | sticky | sticky | sticky |

The row that settles E and F: **leases cannot move onto a push channel.** A
lease exists to decide a worker's fate when it has *stopped* talking, so it must
be renewed by the worker and expire on the server regardless of any connection.
A socket that carries heartbeats still needs lease expiry behind it, so E does
not remove the heartbeat; it adds a second path beside it.

**Recommended: B**, with the snapshot also returned at registration, so a worker
that restarts has its server values before it claims anything rather than one
heartbeat later.

> **As built:** not at registration. Registration runs on the enrollment client,
> before the worker holds a certificate, and `register_status` answers anyone who
> knows a fingerprint -- so a snapshot there would hand a worker's configuration
> to whoever asked. Instead the runner sends one heartbeat before its first
> claim (`Runner::run`), which is authenticated and gets the same result: a
> restarted worker has its server values before it builds anything.

---

## 3. The protocol

### Delivery and effect are separate

With environment-wins, a worker that receives a snapshot with an env-pinned key
has correctly *received* it but deliberately does not run that value. One
version number meaning "applied" would make the server resend the snapshot
forever. So there are two things:

- **`received_revision`** -- the revision of the last snapshot the worker parsed
  and gave every key a status. It only controls retransmission.
- **`effective`** -- per declared key, the value in use, its source (`env`,
  `server`, `env_default`, `default`) and its status. It controls what the UI shows.

The **revision** is the SHA-256 of the canonical serialization of the snapshot:
that worker's stored values, keys sorted. It changes exactly when the snapshot
does, with no counter to keep consistent across restarts or replicas.

### Messages

```text
RegisterRequest   { ...as today, settings: Option<Vec<SettingDecl>> }
RegisterResponse  { ...as today, config: Option<ConfigSnapshot> }

Heartbeat         { ...as today, received_revision: Option<String>,
                    effective: Option<EffectiveConfig> }     // only when it changed
HeartbeatResponse { cancel: Vec<i32>, config: Option<ConfigSnapshot> }  // #[serde(default)]

ConfigSnapshot    { revision: String, settings: BTreeMap<String, String> }
EffectiveConfig   { received_revision: Option<String>,
                    settings: BTreeMap<String, EffectiveSetting> }
EffectiveSetting  { value: Option<String>, source: Env | Server | EnvDefault | Default,
                    status: Applied | Overridden | Unsupported | Rejected,
                    reason: Option<String> }
```

- **A snapshot is always complete**, never a delta, so a worker that missed
  heartbeats needs nothing it did not receive.
- **Settings travel as a string map.** A key the worker does not declare is
  reported `unsupported`; it cannot fail deserialization.
- **Old workers keep working.** `parse_heartbeat_response` falls back to an empty
  response when a body does not deserialize, which would also drop `cancel`. So
  `config` is optional with `#[serde(default)]`, and the server only sends it to
  a worker whose registration carried `settings`. A worker that never declared
  any is shown as "configuration not supported by this worker version".
- **Unreachable server**: nothing changes, as with cancel today. A worker keeps
  its last snapshot; a restarted one gets it at registration before claiming.

### Changes to what the worker registered with: re-register

Concurrency, priority and package affinity are also fields of `RegisterRequest`,
which the server's scheduler reads from the `workers` row. The server does not
need to know that the `concurrency` setting it saved is that same field: after
applying a snapshot, the worker rebuilds its `RegisterRequest` from its new
effective configuration and, **if it differs from the one it last sent,
registers again**. No key is special-cased on either side, and an env-pinned
value is reported as what it is, because registration always carries effective
values.

Re-registering a running worker is safe as registration stands today:

- `worker_store::register_worker` is an upsert keyed on the certificate
  fingerprint. For an existing row it refreshes name, architectures, version,
  kind, affinity, priority and concurrency, and nothing else: no leases, no
  running builds, no approval status.
- The certificate is re-issued only if the worker has none or the CA changed,
  and auto-approval applies only to a pending worker.
- The worker's identity key persists, and it generates a certificate request
  from it at each registration.

The window between applying a snapshot and re-registering is harmless. The
worker acquires a permit before each claim (`runner.rs`), so the server can
never give it more work than its own gate allows; the server's stored
concurrency only decides when a higher-priority worker counts as full, and a
stale value there is bounded by the re-registration, well within the existing
`WORKER_SPILL_DELAY` (60 s) backstop.

### Sequence

```text
Worker                                   Server
  |-- register {settings:[decl...]} ----->  |   stores the declaration
  |-- heartbeat {received: none} -------->  |   (first heartbeat, before any claim)
  |<- {cancel:[], config: rev a1b2} ------  |   worker applies, gives each key a status
  |-- heartbeat {received a1b2, effective}->|   stores the effective config
  |<- {cancel:[]} -----------------------  |
  |                                        |   operator saves concurrency=3: one transaction
  |-- heartbeat {received a1b2} --------->  |   snapshot now hashes to c3d4
  |<- {cancel:[], config: rev c3d4} ------  |
  |   raises its gate to 3                 |
  |-- register {concurrency:3, ...} ----->  |   workers.concurrency = 3
  |-- heartbeat {received c3d4, effective}->|
```

---

## 4. Applying a change on a running worker

### Snapshots per job

A job reads the runtime configuration once, when it starts, and keeps that copy
for its lifetime; a later snapshot is seen whole by the next job. No job runs
with half of one save and half of another, and a build keeps the limits it
started with -- a build sized for one limit and killed by another is a worse
outcome than waiting for the next one.

Each declaration says when it takes effect (`applies`), and the UI shows it:

| Takes effect | Native worker keys |
|---|---|
| **Next job** | build limits, build timeout, builddir budget and floor, cache budgets and TTLs, keyserver |
| **Next loop iteration** | chroot refresh interval, poll interval |
| **Immediately** | concurrency (through the gate below); priority and package affinity (through re-registration); total build limits (below) |

**Total build limits apply to running builds.** They live on the `builds`
cgroup that holds every build, not on a build's own, so rewriting them takes
effect at once -- which is the point of a total, but a lowered memory total
below what the running builds already use makes the kernel reclaim and then
kill one of them. The worker applies it anyway, as an operator asked; the UI
should say so before saving a lower value. (Today the worker applies the totals
only at startup.)

Nothing declared needs a restart. That is part of why paths and users are not
declared.

### Concurrency

The claim loop holds a `tokio::sync::Semaphore` sized once at startup. Resizing
it in place does not work for lowering: while every permit is held,
`forget_permits` removes none, and each finishing build then returns its permit
and restores the old capacity. So the semaphore becomes a small **concurrency
gate**: a target, a running count, and a deficit -- a permit returned while the
running count is above the target is absorbed instead of released. Lowering lets
running builds finish; raising releases permits at once. The worker adjusts the
gate *before* re-registering, so the server never records more capacity than
the worker has.

> **As built** (`aurcache_worker_core::gate`): no semaphore underneath at all,
> just the target and the running count behind a mutex, with a `Notify` woken
> when a build finishes or the target rises. A claim waits while the count is
> at or over the target. That behaves as described -- lowering lets running
> builds finish, raising lets the waiting claim through at once -- with no
> deficit to keep consistent with a permit count.

### CPU limits need the controller first

The worker writes `+cpu` to `cgroup.subtree_control` once, at startup, and only
if `WORKER_BUILD_CPUS` was set. A CPU limit that arrives later would make every
following build fail writing `cpu.max`. So `Hierarchy::for_build` enables the
controllers a job's limits need, on demand, and verifies them; if that fails the
limit is `rejected` with the reason and the previous value kept, rather than
failing builds.

---

## Drain

Drain is a lifecycle state, not a setting: a `draining` column on `workers`, set
by an operator action. The claim query stops offering that worker jobs at once;
running builds finish; the worker needs no message and no new code. As a worker
setting it would be wrong twice over -- a worker could boot drained from its
environment, and it would take a heartbeat and a re-registration to stop claims
that the server can stop itself.

> **As built, as pause and resume:** "drain" names something else wherever an
> operator has met it -- Kubernetes and Nomad both *evict or migrate* running
> work on a drain, and call "take nothing new" cordoning or ineligibility --
> so it would read as cutting builds short, the opposite of what this does.
> Pause is what GitLab calls exactly this for its runners.
>
> `workers.paused` (`m20260925_000000_worker_paused`), set with
> `POST /workers/<id>/pause` and `/resume`, and `aurcache-cli worker
> pause|resume`. What an operator reads is **Stop intake** and **Resume
> intake**: about the worker rather than the builds, since new builds are not
> stopped, only sent to other workers. The claim query gives a paused worker
> nothing, and a paused worker no longer counts as available for priority
> hold-back, so a lower-priority worker does not wait on one that will never
> claim. The hard routing rules are unchanged: a package reserved to a paused
> worker, or an arch only it builds natively, waits for it, and the waiting
> reason says so (`WaitingReason::Paused`). Revoking clears the flag, so a
> machine approved again later builds straight away; restarting does not.

---

## Recommendation

**Phase 1 -- visibility, then per-worker configuration over the heartbeat (B).**

1. ~~`ValueKind` and its parsers in `aurcache-common`; `SettingDecl` for the native
   worker's policy keys. Retire the dead server settings `max_concurrent_builds`
   and `builder_image` the way `cpu_limit`/`memory_limit` were. The worker reads
   `<env_var>_DEFAULT` for every declared setting.~~ **Done.**
2. ~~**Read-only first.** The worker sends its declaration at registration and
   reports `effective` (value, source, status) in the heartbeat when it changes;
   the server stores both and the Workers page shows them, with parse errors
   flagged. Useful on its own; ship it first.~~ **Done.**

   As built: the spec tables are `aurcache_worker_core::settings` (protocol) and
   `aurcache_worker::settings` (the chroot executor), resolved once at startup
   into `CoreConfig::settings`, which every config field then reads -- so the
   value the worker runs and the value it reports cannot differ. The declaration
   and the report are stored as JSON on the `workers` row and served from
   `GET /workers/<id>/config`.

   The UI is the worker detail page of step 6, read-only: `/worker/<name>`,
   reached by following a worker from the fleet list, rendering the declaration
   generically by category and kind. The list stays as it was apart from a count
   of refused settings, which is all it needs to flag a row worth opening.

   The URL carries the name rather than the id because that is what an operator
   has in hand, but a name is not unique and deliberately is not made so: a
   worker is called whatever its machine reports, a retired row keeps its name
   for ever, and a machine replaced by another of the same hostname is the
   ordinary case. So a shared name resolves to a choice rather than a guess, and
   `/workers/by-cert/<fingerprint>` is the way past it -- the fingerprint being
   the identity the whole protocol is already keyed on.
3. ~~`worker_settings` table, dump/restore, transactional `PATCH` per worker, one
   activity entry per save.~~ **Done.**

   As built: `worker_settings (worker_id, key, value)` with `UNIQUE (worker_id,
   key)` and a cascading foreign key (`m20260924_000000_worker_settings`).
   `PATCH /workers/<id>/config` takes `{settings: {key: value | null}}`, checks
   every value against the stored declaration before writing any, and writes
   the set in one transaction (`worker_store::save_worker_settings`); `null`
   removes a value and needs no declaration, so one for a key a worker has
   dropped can still be cleared. One `worker.settings_saved` entry per save
   names the keys set and reset, not the values. Dumps carry the values on each
   worker (`DumpWorker::settings`); restore writes them for a worker it creates,
   keyed to its new row, and leaves an already-trusted worker's alone.
4. ~~Snapshot delivery: `config` in the registration and heartbeat responses,
   `received_revision` back.~~ **Done**, over the heartbeat only (see
   [Recommended](#compared)). The revision is a SHA-256 of the values as a
   key-ordered JSON map, computed only on the server.
5. ~~Worker application: per-job snapshots, the concurrency gate, on-demand cgroup
   controllers, per-key status with rejected values keeping the previous one,
   re-registration when the registration request changes.~~ **Done.**

   As built: `WorkerSettings::with_snapshot` layers the values between the pin
   and `_DEFAULT`; the runner hands the result to `Executor::reconfigure`, which
   may refuse what the machine cannot honour (`WorkerSettings::refused` keeps the
   previous value) and returns what is in force. The runner and the chroot
   executor each hold their configuration behind a lock and swap it whole; a job
   takes the current one when it starts. Only values from the server are refused
   by the executor: a limit pinned in the environment that cannot be enforced
   keeps its startup behaviour of refusing builds. `Hierarchy::for_build` enables
   the `cpu` controller when a build's limits need it, and new totals are
   written to `builds/` as soon as they arrive. Re-registration is checked on
   every heartbeat and retried until the server takes it.
6. ~~UI: the worker detail page (now read-only) grows the editing half -- a field
   per setting, per-key status, and env-pinned rows naming the variable and the
   way to unpin them. The shipped compose files and the CLI's generated ones
   write policy as `_DEFAULT` variables.~~ **Done.**

   As built: edits are staged and saved together; a pinned row's field is
   disabled; a staged change says when it takes effect, including the warning
   for settings applied to running builds; values for keys no longer declared
   are listed to be cleared; and the page says when the worker has not yet taken
   the latest save. `aurcache-cli worker config <id> [--set k=v] [--reset k]` does
   the same from a terminal.

**Later, if wanted:** fleet defaults, on the terms in
[Per worker only, for now](#per-worker-only-for-now); ~~declarations for the
legacy container worker~~ **done**.

> **As built** (`aurcache_worker_docker::settings`): `cpu_limit` and
> `memory_limit`, in the executor's own units -- `CPU_LIMIT` in milli-CPUs and
> `MEMORY_LIMIT` in MB, read under their pre-worker names -- rather than the
> chroot executor's `build_cpus` and `build_memory_max` translated onto it.
> `BUILDER_IMAGE` and `BUILD_ARTIFACT_DIR` are deliberately not declared: the
> image decides the user and privileges a build runs with, and the directory is
> a host path bound into every container. The executor holds its configuration
> the way the chroot one does, one copy per build, and refuses nothing on
> delivery: Docker enforces whatever limit it is given.

**Phase 2, only if latency is felt -- SSE (D) for server-to-worker events.** Stop
and "configuration changed" become events; the heartbeat stays exactly as it is
for leases, and is the fallback if the stream is down, so correctness never
depends on the stream. D over C for the lower bookkeeping; D over E because it
adds the missing direction without a second protocol for the direction that
already works.

**Not recommended now: WebSocket or gRPC.** Revisit if the worker-to-server
direction becomes a problem too -- log streaming at a scale where one POST per
batch hurts -- since that, not configuration, is what would justify a
bidirectional connection.

---

## History

### First revision: the reviews

| Raised | By | Outcome |
|---|---|---|
| One `config_version` cannot mean both received and effective | codex | Adopted: `received_revision` and `effective` |
| Revision as a content hash of the server's payload | both | Adopted |
| Saves must be atomic; snapshots complete, not deltas | codex | Adopted |
| Rejected values must not fall back to a weaker default; per-key status | both | Adopted, keeping the previous usable value |
| Server resolves fleet vs per-worker layers | both | Adopted, then made moot: no fleet defaults |
| Priority/affinity must not wait for a restart | both | Adopted; second revision delivers them by re-registration |
| Semaphore cannot be resized in place | both | Adopted: concurrency gate |
| CPU controller only enabled at startup | both | Adopted: enabled on demand |
| Forward compatibility of `HeartbeatResponse` | both | Adopted |
| Env-wins leaves existing workers unmanageable | both | Kept env-wins; added the adoption path |
| Partial unique indexes for the fleet `NULL` scope | both | Adopted, then made moot: no fleet scope |
| Cascade and dump/restore | both | Adopted |
| Allowlist gaps (repo host/URL, keyserver, overlay, heartbeat and poll intervals); mirrorlist conflicts with per-arch mirrorlists | review | Adopted, now as what the native worker declares |
| Open questions: affinity per worker only, one audit entry per save, drain as server state, legacy worker env-only | both | Adopted; the legacy worker can now declare its own settings instead |

Where the reviews did not match the code, for the record: `forget_permits` does
not panic when no permits are free (it returns how many it removed, which is
none); the mirrorlist is delivered at registration, not per build; and duplicate
`NULL` rows under a unique constraint are not specific to SQLite -- Postgres
allows them too.

### Second revision: declared settings

- The server-side `WorkerSetting` allowlist became a declaration each worker
  sends, with a shared `ValueKind` vocabulary as the only common definition.
  Server and workers can then be upgraded independently, and different worker
  implementations expose different settings.
- Fleet defaults were dropped for now. They were the only feature that needed
  workers to agree on what a key means.
- Priority, affinity and concurrency reach the scheduler by the worker
  re-registering when its registration request changes, instead of the server
  interpreting those keys. The reviews' objection to re-registration was to
  waiting for a restart; a worker re-registering itself is safe as registration
  stands (see [the protocol](#changes-to-what-the-worker-registered-with-re-register)).
  The concurrency race the reviews described does not arise, because the worker
  acquires a permit before it claims.

### Third revision: environment defaults

- A `<env_var>_DEFAULT` variable sets a default the server may override, below
  the server value and above the built-in default; the plain variable still
  pins. Handing a setting to the server became a rename that keeps the machine's
  value, instead of a deletion that dropped it to the built-in default, and
  shipped templates write defaults instead of commenting policy out.

### Implementation

Steps 3-6 built as described above, with the two divergences noted where they
apply: no snapshot at registration, and a gate without a semaphore. The
`concurrency` and `WORKER_TOTAL_BUILD_*` declarations now say they apply
immediately, which is what the table in [Snapshots per job](#snapshots-per-job)
always said they would.
