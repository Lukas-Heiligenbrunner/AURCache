---
sidebar_position: 2
---

# Worker Configuration

:::note Upgrading from a single container
If you are still running AURCache as one container, it keeps working through the
[hybrid compatibility image](./hybrid.md),
which is deprecated. Everything on this page applies to a worker you run
yourself.
:::


A build worker is a long-lived process that enrolls with AURCache, claims build
jobs, builds each package in its own `devtools` chroot, and uploads the result.
A worker starts from its environment variables. Its policy and tuning settings —
concurrency, build limits, cache budgets, timeouts — can then be set for it from
AURCache, on the worker's own page, and reach it without a restart. Everything
else (its identity, its paths, what a build may reach) stays on the machine.

## Seeing what a worker is running

A worker tells the server which of its settings can be configured, and what each
of them resolved to on that machine. Follow a worker from the **Workers** list to
its own page — `/worker/<name>` — to see both: the value in force, where it came
from, and the variable that set it.

Worker names are not required to be unique: a worker is called whatever its
machine reports (`WORKER_NAME`, or the hostname), and a revoked worker keeps its
row so old builds still name the machine that ran them. Where two workers share
a name the page asks which you meant, and each is also reachable by the
fingerprint that is its real identity.

This is where a value that did not parse shows up. `WORKER_BUILDDIR_MAX_BYTES`
set to something the worker cannot read does not stop it starting — it falls back
and carries on — but the page then says the value was refused and names what is
running instead, and the worker's row is flagged in the list. Before, the only
trace was a line in that machine's journal.

## Setting values from AURCache

Each setting on a worker's page has a field. Change as many as you need, then
**Save**: the whole set is saved as one change, so a worker never picks up half
of it — fewer builds with more memory each cannot arrive as more builds with
more memory each. The CLI does the same:

```bash
aurcli worker config 3                                  # show
aurcli worker config 3 --set concurrency=2 --set build_memory_max=48G
aurcli worker config 3 --reset build_timeout            # back to the worker's own
```

A value is checked against what that worker says it accepts before it is saved,
so `1.5 bananas` for a size is refused where you typed it. The worker picks the
save up on its next heartbeat (every 15 seconds), or when it next checks in if
it is offline, and its page then shows the value as *set here*. When it takes
effect depends on the setting, and the page says which:

| Takes effect | Settings |
|---|---|
| At once | concurrency (running builds finish; nothing new starts over the new limit), priority and package affinity, the `WORKER_TOTAL_BUILD_*` totals |
| From the next build | per-build limits, build timeout, keyserver — builds already running keep what they started with |
| At its next pass | cache budgets and TTLs, build-tree budgets, chroot refresh and poll intervals |

The totals apply to builds already running: lowering the total memory below
what they are using makes the kernel reclaim and then kill one of them.

A worker can still refuse a value AURCache accepted — a CPU limit on a host
whose cgroup cannot take one, say. It then keeps the value it was running (a
refusal never loosens a limit), and the page shows the refusal and why.

Removing a value (**Reset**) returns the setting to the worker's own: its
`_DEFAULT` variable if it has one, otherwise its built-in default. A value set
for a setting the worker no longer offers — renamed or removed in an upgrade —
is kept and listed separately rather than deleted, so you can clear it.

Values set here are included in a backup (**Settings → Backup**), with the
worker they belong to, and restored with it.

### `_DEFAULT`: a value the server may take over

Every setting that page lists reads two variables:

| In the worker's environment | Meaning |
|---|---|
| `WORKER_CONCURRENCY=4` | **Pin.** This machine runs 4, whatever anything else says. |
| `WORKER_CONCURRENCY_DEFAULT=4` | **Default.** This machine runs 4 until a value is set for it on the server. |

A value set in AURCache beats `_DEFAULT` but not the pin: the order is pin, then
AURCache, then `_DEFAULT`, then the built-in default. So a machine can keep a
value out of AURCache's hands — its memory limit, say — while leaving its
concurrency to be managed centrally. A pinned setting's field is disabled, and
the page names the variable doing the pinning.

Renaming a variable is the whole of the handover: `WORKER_CONCURRENCY=2`
becomes `WORKER_CONCURRENCY_DEFAULT=2`, and the machine keeps running 2 until a
value is set for it. The compose files AURCache ships, and those `aurcli
setup` writes, use the `_DEFAULT` names. A worker older than this feature ignores
`_DEFAULT` and runs its built-in default instead, so upgrade workers before
renaming their variables.

Only the settings that page lists read `_DEFAULT`. The ones that decide what a
build can reach — the chroot directory, the bind mounts, the build user — are
deliberately not among them: a worker runs `devtools` as root, so those stay on
the machine, where they always were.

## Identity and capabilities

| Variable | Type | Description | Default |
|---|---|---|---|
| `WORKER_NAME` | String | Name shown on the Workers page | hostname |
| `WORKER_ARCHES` | String | Architectures this worker builds, comma or space separated | host architecture |
| `WORKER_CONCURRENCY` | Integer | Maximum builds at once | 1 |

:::note Raising concurrency
Each build is already parallel — `MAKEFLAGS=-j$(nproc)` is set inside every
build — so `WORKER_CONCURRENCY=N` runs up to N × CPU-count compiler processes,
and each concurrent build additionally holds its own chroot copy and package
cache. Raise it when you have memory and disk headroom, not simply because the
machine has cores; two or three is a large increase for most hosts.
:::

## Building for another architecture

A worker builds in a chroot of its own architecture, so it builds for that
architecture only.

A foreign-architecture worker is the *same* worker image run emulated, with
`platform: linux/arm64` (or `linux/arm/v7`). Docker uses binfmt_misc to register
the qemu interpreter for that platform, so from the worker's point of view it is
building natively and the build path is identical to a native worker's.

The bundled compose file ships this behind a profile:

```bash
docker compose --profile arm up -d
```

which also runs `tonistiigi/binfmt` once to register the handlers on the host
kernel. Emulated builds are slow, so that worker is configured with
`WORKER_CONCURRENCY=1`; if you later add a native aarch64 machine, give it a
higher [priority](./routing.md) and aarch64 jobs will prefer it,
falling back to emulation only when it is busy or offline.

Supported platforms are `x86_64`, `aarch64` and `armv7` — the Arch Linux base
image the worker is built on exists only for those. Emulation itself only runs
one way: qemu user-mode runs on an x86_64 host, so cross builds always start
there.

If a worker fails to start with `exec container process (missing dynamic
library?)`, its qemu/binfmt setup is misconfigured: `binfmt_misc` must be
enabled in the kernel (`cat /proc/sys/fs/binfmt_misc/status`), enabling it with
`mount binfmt_misc -t binfmt_misc /proc/sys/fs/binfmt_misc` if not.

## Swarm and other runtimes

In Docker Swarm the worker is an ordinary service with the same needs: the
permissions a chroot build requires — `privileged: true`, or `SYS_ADMIN` /
`SYS_PTRACE` with seccomp and AppArmor unconfined — plus a writable tmpfs at
`/run`. Constrain workers to particular nodes with the usual Swarm `placement`
rules, which pairs well with [routing](./routing.md) when a package must be
built on a specific machine.

## Routing

| Variable | Type | Description | Default |
|---|---|---|---|
| `WORKER_PACKAGES` | String | Exact pkgbase names this worker is provisioned for | empty |
| `WORKER_PRIORITY` | Integer | Scheduling preference; higher wins | 0 |

See [Routing builds to specific workers](./routing.md).

## Mirrorlist

By default a worker needs none of this: the server sends the mirrorlist for the
build's architecture with each job, and a worker that gets none uses its image's
own `/etc/pacman.d/mirrorlist`.

Set one of these when this worker's mirrors beat the server's — a worker on
other hardware, or across a slow link from the mirror the server prefers.

| Variable | Type | Description | Default |
|---|---|---|---|
| `WORKER_MIRRORLIST_SERVERS` | String | Semicolon-separated mirror URLs, used instead of the server's | empty |
| `WORKER_MIRRORLIST_FILE` | Path | A ready mirrorlist file inside the container | empty |

`WORKER_MIRRORLIST_SERVERS` wins if both are set. Either one makes the worker
tell the server not to send a mirrorlist at all, so nothing is transferred that
would only be discarded.

See [Mirrorlist](../Configuration/mirrorlist.md) for the server side.

## Build credentials

| Variable | Type | Description | Default |
|---|---|---|---|
| `WORKER_GIT_SSH_KEY` | Path | SSH key for authenticated sources. When set, no key is generated | generate one |
| `WORKER_SSH_KNOWN_HOSTS` | Path | `known_hosts` to trust when fetching sources | accept on first use |
| `WORKER_BIND_MOUNTS` | String | Extra `host:chroot` mounts, comma separated | empty |

See [Build credentials](./credentials.md).

## Caches

Workers keep two caches: downloaded upstream sources (`SRCDEST` — tarballs and
VCS checkouts) and downloaded pacman packages. They are budgeted separately
because they differ enormously in size and in what it costs to refill them: a
few large VCS checkouts sharing one budget would evict every cached package,
leaving each build to re-download its dependencies.

| Variable | Type | Description | Default |
|---|---|---|---|
| `WORKER_CACHE_MAX_SIZE` | Size | **Total** cache budget, split evenly between the two pools | `20G` |
| `WORKER_SRCCACHE_MAX_SIZE` | Size | Source budget; overrides half the total | half of total |
| `WORKER_PKGCACHE_MAX_SIZE` | Size | Package budget; overrides half the total | half of total |
| `WORKER_CACHE_TTL` | Duration | Evict sources unused for this long (`0` disables) | `30d` |
| `WORKER_PKGCACHE_TTL` | Duration | Evict packages older than this (`0` disables) | `0` |
| `WORKER_BUILDDIR_MAX_BYTES` | Size | Budget for kept build trees, for packages with a persistent build directory | `200G` |
| `WORKER_BUILDDIR_MIN_FREE` | Size | Free space to keep in the storage pool when deciding whether to drop kept build trees | `50G` |

The caches live in the worker's [storage pool](./storage-pool.md),
in a `cache` subvolume beside the chroots, so they count against
`WORKER_DISK_MAX` together with the base chroot and the running builds. Each
package's sources and each kept build tree is a subvolume of its own: its size
is read from btrfs rather than by walking it, and evicting it is instant
however many files it holds. A build is given only its own kept tree. The
budgets here are what eviction keeps them to between builds; leave room under
`WORKER_DISK_MAX` for at least one build's `WORKER_BUILD_DISK_MAX` on top of
them, or a build may find the pool full before its own quota is reached.

Sizes are read the way coreutils reads them: a plain number is bytes, a bare
unit or an `iB` unit is binary, and a `B` unit is decimal -- `450G` and
`450GiB` are both 450&nbsp;GiB, while `450GB` is 450&nbsp;×&nbsp;10⁹ bytes.
Units run from `K` to `T`, in any case, with or without a space before them.
Setting a budget to `0` disables size-based eviction for that pool.

A value the worker cannot read, here or in any other setting, is logged as a
warning at startup and replaced by the default -- check the log after
changing one.

Setting both per-pool budgets makes the split ratio configurable without a
separate knob for it. A pinned pool always wins: the total is a default for
whatever is left unset, not a cap enforced over values you wrote down.

**Package TTL defaults to off** deliberately. A cached package's timestamp is
when it was *downloaded*, not when it was last used — pacman does not touch it
on a cache hit — so age-evicting would discard a package used in every build
simply for being old. Size pressure is the honest bound for that pool.

## Resource limits

| Variable | Type | Description | Default |
|---|---|---|---|
| `WORKER_BUILD_MEMORY_MAX` | Size | Memory each build may use; past it, the kernel kills processes in the build | unlimited |
| `WORKER_BUILD_SWAP_MAX` | Size | Swap each build may use besides that memory | `0` when `WORKER_BUILD_MEMORY_MAX` is set, otherwise unlimited |
| `WORKER_BUILD_CPUS` | Number | CPU time each build may use, in cores; fractions such as `2.5` are allowed | unlimited |
| `WORKER_TOTAL_BUILD_MEMORY_MAX` | Size | Memory all of this worker's builds may use together | unlimited |
| `WORKER_TOTAL_BUILD_SWAP_MAX` | Size | Swap all builds may use together besides that memory | `0` when `WORKER_TOTAL_BUILD_MEMORY_MAX` is set, otherwise unlimited |
| `WORKER_TOTAL_BUILD_CPUS` | Number | CPU time all builds may use together, in cores | unlimited |

The `WORKER_BUILD_*` limits apply to **each build**, so a worker running
`WORKER_CONCURRENCY` builds can use that many times as much. The
`WORKER_TOTAL_BUILD_*` limits bound what the builds use **between them**. The two
nest, and can be combined: with `WORKER_BUILD_MEMORY_MAX=32G` and
`WORKER_TOTAL_BUILD_MEMORY_MAX=48G`, no single build gets more than 32G, and two
running at once share 48G.

A total does not limit how many builds start -- `WORKER_CONCURRENCY` does. Two
builds that each need 30G under a 48G total both start, and the one that
allocates past the total is killed. A build can therefore be killed while under
its own limit; its failure reason says which limit was reached:

| Reason names | What was reached |
|---|---|
| `WORKER_BUILD_MEMORY_MAX` | the build's own limit |
| `WORKER_TOTAL_BUILD_MEMORY_MAX` | the total, shared with the builds running beside it |
| neither | a limit outside the worker: the machine's memory, or one on what runs the worker |

Neither limit includes the worker process itself, which sits beside the builds
rather than under the total, so reaching the total can only ever stop a build.
To bound literally everything, limit what runs the worker instead:
`MemoryMax=`/`CPUQuota=` on the systemd unit, `--memory`/`--cpus` on its
container. The totals are applied when the worker starts; changing one takes a
restart.

**A memory limit limits swap too.** On its own, a cgroup's memory limit only
bounds RAM: a build over it is pushed out to swap rather than stopped, which on
a machine with swap means no limit at all. So setting `WORKER_BUILD_MEMORY_MAX`
also sets the build's swap allowance to zero, and a build that needs more is
killed. Set `WORKER_BUILD_SWAP_MAX` to allow some. The total works the same way,
with `WORKER_TOTAL_BUILD_SWAP_MAX`. A build killed this way fails
with a reason that names the limit, rather than a bare exit code.

**A CPU limit also limits parallelism.** `nproc` inside the build still counts
every core, so the worker lowers `MAKEFLAGS`, `OMP_NUM_THREADS` and
`CARGO_BUILD_JOBS` to the number of CPUs allowed -- the smaller of
`WORKER_BUILD_CPUS` and `WORKER_TOTAL_BUILD_CPUS` when both are set. Without that, a build limited
to 6 CPUs on a 24-core machine would start 24 compilers, each holding its own
memory. A build that picks its parallelism some other way is still held to the
limit, only less efficiently.

The limits are enforced with cgroups -- each build's own, under a `builds`
cgroup that holds the total -- which need the same thing the build's memory
figure does: a privileged container, or `Delegate=yes` on the systemd unit (the
packaged unit has it). A worker that has limits configured
but cannot enforce them **refuses every build** and says so at startup, rather
than running builds without the limits it was given.

The legacy container builder keeps its own `CPU_LIMIT` (milli-CPUs) and
`MEMORY_LIMIT` (MB, negative for unlimited). Like the settings above, they are
listed on the worker's page and can be set there, and read `CPU_LIMIT_DEFAULT`
and `MEMORY_LIMIT_DEFAULT` as defaults AURCache may override; the plain names
still pin. Its builder image and build directory are not settable from
AURCache.

## Locations

| Variable | Type | Description | Default |
|---|---|---|---|
| `WORKER_DATA_DIR` | Path | Worker identity, credentials, chroots | `/var/lib/aurcache-worker` |
| `WORKER_CHROOT_DIR` | Path | Where the storage pool's image and mount point go (see below) | `<data dir>/chroot` |
| `WORKER_CHROOT_REFRESH_INTERVAL` | Duration | How long a `pacman -Syu`'d base chroot counts as current (`0` refreshes before every build) | `15m` |

The base chroot is brought up to date with `pacman -Syu` before a build, but
not more often than `WORKER_CHROOT_REFRESH_INTERVAL`. The refresh takes around
13 seconds, so paying it per build delayed every build start in a burst; measured over a day on one
worker, 23 of 26 refreshes upgraded nothing at all, because Arch's repositories
move a few times a day rather than a few times an hour. Lower it if you would
rather have the newest dependencies than the fastest starts; `0` restores the
old behaviour.

`WORKER_DATA_DIR` **must be a persisted volume.** It holds the worker's identity
and its generated SSH key; if it is lost, the worker re-enrolls as a new,
unapproved worker and generates a new key, which the remote will no longer
accept.

## Disk quota and the storage pool

| Variable | Type | Description | Default |
|---|---|---|---|
| `WORKER_BUILD_DISK_MAX` | Size | Disk one build may write: its chroot, its sources, the packages it makes | `50G` |
| `WORKER_DISK_MAX` | Size | Everything the worker stores in its pool: the base chroot, the caches and every running build | `200G` |
| `WORKER_POOL` | Path | What backs the pool: unset for an image file, a block device, or a dedicated btrfs mount | unset |
| `WORKER_DISK_RESERVE` | Bool | Allocate the whole image up front instead of as builds write | `false` |
| `WORKER_KEEP_FAILED` | Duration | How long a failed build's chroot is kept for inspection | unset (keeps nothing) |

Every chroot, build and cache lives in the worker's **storage pool**, a btrfs
filesystem with quotas. The kernel enforces both limits as builds write: a
build that tries to fill the disk fails with "Disk quota exceeded" and nothing
outside the pool is touched. By default the pool is a sparse image file that
needs no setup. On ZFS, a zvol is better.

[Storage pool](./storage-pool.md) covers the three backings (an image file, a
zvol or partition, a dedicated btrfs filesystem), their tuning, sizing, and
resizing.

When `WORKER_KEEP_FAILED` is set, a failed build's chroot is kept at
`<pool>/kept-<id>` (with its workdir beside it, and its build tree at
`<pool>/kept-<id>.build` when the build kept one) instead of deleted, so a
failure can be inspected in the state it died in. The build's page says where
and until when, and shows the one `systemd-nspawn` command that enters it.
Canceled, timed-out and out-of-disk builds are never kept, and one-shot
builds neither; a kept build counts against `WORKER_DISK_MAX` and is deleted
early, oldest first, when the pool needs the room.

## Timing

| Variable | Type | Description | Default |
|---|---|---|---|
| `WORKER_BUILD_TIMEOUT` | Duration | Kill a build after this long (`0` disables) | `3h` |
| `WORKER_POLL_INTERVAL` | Duration | Time between job claims when idle | `10s` |
| `WORKER_HEARTBEAT_INTERVAL` | Duration | Time between heartbeats | `15s` |
| `LEASE_TTL` | Duration | How long before a silent worker's build is requeued | `60s` |
| `WORKER_KEYSERVER` | String | Keyserver for `validpgpkeys` | `hkps://keyserver.ubuntu.com` |

Durations are a plain number of seconds, or terms with a unit that add up:
`90s`, `15m`, `3h`, `1h30m`, `2h 30min`, `30d`, `2w`. Units are `s`, `m` (minutes),
`h`, `d` and `w`, or spelled out (`min`, `hours`, `days`), in any case. Like
sizes, a value that cannot be read is logged at startup and the default used.

Very large packages need `WORKER_BUILD_TIMEOUT` raised — the three-hour default
is generous for ordinary packages and nowhere near enough for something like a
browser or a game engine.

## Connecting to AURCache

| Variable | Type | Description | Default |
|---|---|---|---|
| `AURCACHE_URL` | String | Worker protocol endpoint (HTTPS + mTLS) | `https://localhost:8080` |
| `AURCACHE_SERVER_CA_FINGERPRINT` | String | Pin the server's CA (SHA-256 hex) | trust on first use |
| `AURCACHE_ENROLLMENT_DIR` | Path | Shared volume for zero-touch approval | `/enroll` |
| `AURCACHE_ENROLLMENT_TOKEN` | String | Shared secret for non-interactive approval | none |

## Server-side settings

These are set on the AURCache server, not the worker.

| Variable | Type | Description | Default |
|---|---|---|---|
| `AURCACHE_WORKER_PORT` | Integer | Port for the worker protocol listener | 8083 |
| `AURCACHE_WORKER_IMAGE` | String | Worker image named in the copy-and-run command the **Workers** page shows when no worker exists yet | `ghcr.io/lukas-heiligenbrunner/aurcache-worker:latest` |
| `WORKER_SPILL_DELAY` | Duration | How long before priority stops holding a job back | `60s` |
| `WORKER_LIVENESS_TIMEOUT` | Duration | How long before a quiet worker stops counting as available | `60s` |
| `MAX_ATTEMPTS` | Integer | Requeues before a build is failed for good | 3 |
| `WORKER_CERT_VALIDITY_DAYS` | Integer | Lifetime of an issued worker certificate | 3650 |

Set `AURCACHE_WORKER_IMAGE` when you publish the worker image to your own
registry or want the suggested command to pin a tag — it only changes what that
one command displays, nothing about how workers connect.

Worker certificates are long-lived on purpose. A certificate grants nothing on
its own — authorization is the worker's approval status, which an operator can
revoke at any moment — so a short lifetime would add no security and would
eventually break a worker with no renewal path.
