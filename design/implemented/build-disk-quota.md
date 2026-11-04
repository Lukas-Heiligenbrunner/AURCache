# Disk quotas for builds

Plan for bounding the disk a build can use *while it runs*, so a PKGBUILD that
sets out to fill the disk fails its own build instead of taking the worker's
host down with it. The same mechanism bounds everything else the worker
stores, so one figure caps the worker's whole footprint.

Status: **Implemented** · Last updated: 2026-09-25

What shipped:

- The server-side parse writes nothing, and is bounded: 60s, 8 MiB of output,
  its whole process group killed on overrun.
- The server never follows a package's symlinks: archives store them as
  links, `.SRCINFO`/`PKGBUILD` are read only as regular files, and a
  subfolder resolving outside the checkout is refused. The worker never
  uploads a symlinked "package".
- The `aurcache-chroot` crate: the pool on an image, a device or a mount;
  per-build subvolumes and qgroups; the total; grow, online shrink, and
  drain-and-recreate when an image cannot shrink; sweep and group cleanup.
  An image is attached by `mount -o loop`, so its loop device autoclears when
  a container holding it goes.
- The worker builds in pool snapshots with `WORKER_BUILD_DISK_MAX` and
  `WORKER_DISK_MAX`, reports a quota failure, and stops claiming while a sparse
  image's host lacks room. devtools' `WORKDIR` (where the host-side
  `makepkg --verifysource` writes) is in the build's own subvolume, through
  `TMPDIR`. The overlay and copy strategies are gone.
- The caches live in one `cache` subvolume in the pool, under the total
  (`WORKER_CACHE_DIR` and the separate cache volume are gone). Each package's
  sources and each kept build tree is a subvolume inside it (created by the
  worker, which the kernel counts under the total on its own; the worker
  assigns it where it does not): measured by its quota group, evicted by
  `subvolume delete`. Plain directories from before keep the old walk and
  `rm`. A build is bound only its own tree, at `/build/<pkgbase>`.
- A device's or a mount's filesystem is fitted to the total online (shrunk
  when it comes down, grown back when it goes up, never past the device); the
  device itself is the operator's, and the smallest safe size is logged.
- A worker that stops claiming says so at error severity.
- ZFS guidance where the setup is made: the compose file and `setup worker`,
  plus a startup log when the image is on ZFS.
- Each build reports its disk use part by part -- chroot, working space,
  sources, kept tree -- stored on the build and shown on its page.
- A patched source keeps its symlinks, directories and file modes.
- Operator docs (`docs/docs/workers/storage-pool.md`) for the three backings
  and their tuning; `setup worker` and `setup compose` ask for the backing
  and give zvol tuning.
- Running on both workers, each pool on a zvol: freyja's over iSCSI, mounted
  as a dedicated btrfs filesystem, and the TrueNAS builder's as a device.

Decided: the caches' budgets stay eviction targets, not hard qgroup limits.
The total is the hard bound they were approximating; the budgets only decide
what to prune first.

Deferred: per-package quotas (a qgroup limit on the package's subvolume, now
that it has one), and a real build-time comparison (item 6 below). Further
uses of the pool's snapshots are in `design/proposed/btrfs-snapshots.md`.

---

## The problem

PKGBUILD code runs in two places, and neither had a bound on what it writes.

**On the server**, parsing a PKGBUILD sources it. The parse was confined by
`aurcache-sandbox` to its own directory for writes, and for a git source that
directory is the live checkout. So a parse could write as much as it liked to the
server's disk (the database, the repository), and could also rewrite the checkout
after its metadata had been read and before it was archived for the workers.

**On a worker**, a build can write to everything bound into its chroot:

| Path | What | Filesystem today |
|---|---|---|
| `/` | chroot copy (btrfs snapshot, rsync copy, or overlay upper) | chroot dir |
| `/build` | build tree, in the copy or a kept one | chroot dir / cache dir |
| `/pkgdest`, cwd | extracted source, finished packages | data dir |
| `/var/cache/pacman/pkg` | the job's private pacman cache | cache dir |
| `/srcdest` | the pkgbase's source cache | cache dir |

A build can get root inside its chroot (`sudo pacman -U` of a package it made
itself), so everything bound in is writable, whatever its permissions.

Nothing bounded any of these during a build. `WORKER_BUILDDIR_MAX_BYTES`,
`WORKER_BUILDDIR_MIN_FREE` and the cache eviction all run *between* builds.
`cache.rs` records one build tree reaching 333G, and `build.rs` records a disk
filling up and taking a four-hour build with it.

### Why the bound has to be the kernel's

A watchdog that polls free space and kills the offender looks attractive,
because it needs nothing from the filesystem. It fails against a build that is
actually trying: `fallocate -l 1T x` allocates a terabyte in one syscall, and
the disk is full before the next poll. Blocking `fallocate` narrows the gap, but
plain writes run at GB/s and the attacker picks the timing. So the limit has to
return `EDQUOT`/`ENOSPC` at the moment of the write.

## Server: the parse writes nothing (done)

The parse needs to *read* its PKGBUILD, not write anything:
`alpm-pkgbuild-bridge` only sources the file and prints. `Bridge::command`
(`aurcache-utils/src/pkgbuild.rs`) now grants the PKGBUILD's directory with
`--read` instead of `--allow`, so the only writable path left is `/dev/null`.
The directory is still granted explicitly, because the server's own working
directory (protected) holds the checkouts.

The one casualty is a top-level heredoc too large for a pipe (over 64K), which
bash spills to `$TMPDIR`. That statement fails and the parse carries on.
Measured against real PKGBUILDs and the upstream bridge script, nothing else
changed. `a_parse_cannot_read_protected_files_or_write_anywhere` checks it end
to end: no write outside the directory, no write inside it, and a parse from
inside a protected directory still works.

This also closes the checkout-rewriting path above.

The other server-side sink PKGBUILD output reaches is artifact upload, and that
is already bounded by `max_artifact_size`.

## Worker: everything it writes lives in one btrfs pool

The worker keeps all its storage in a **pool**: one btrfs filesystem the worker
owns, with [simple quotas][squota] enabled. Builds, the base chroot and every
cache all live there, so one limit bounds the whole worker, which is the figure
an operator actually cares about.

- The pool's top qgroup `2/0` is limited to `WORKER_DISK_MAX`: the worker's
  whole footprint. (btrfs only allows a qgroup's parent to be at a higher
  level, so the total is the level-2 group and everything else hangs below it.)
- Each build gets its own subvolumes, grouped under a qgroup limited to
  `WORKER_BUILD_DISK_MAX`.
- Each cache gets a qgroup of its own, limited by the size setting it already
  has.

A write past any of these limits fails with `EDQUOT` at once, `fallocate`
included, whatever filesystem the host has.

[squota]: https://btrfs.readthedocs.io/en/latest/Qgroups.html#simple-quotas-squota

Simple quotas (kernel 6.7+) charge an extent to the subvolume that *wrote* it,
with none of the backref walking that makes classic qgroups slow once snapshots
share extents. That is exactly what's needed here: a build's snapshot of the
base chroot shares every base extent, and only what the build writes itself
should count against it.

### What backs the pool

The worker has to run on hosts whose own filesystems cannot enforce any of this:
the local worker's chroot and cache are btrfs, the TrueNAS worker's volumes are
ZFS datasets seen from inside a container, and a user might have ext4. So the
pool is a filesystem the worker creates itself, on whatever the operator can
give it (`WORKER_POOL`):

1. **An existing, dedicated btrfs mount**, e.g. `/var/lib/aurcache-chroot`
   locally, which is already a btrfs filesystem of its own. No loop device and
   no nested filesystem; the worker runs `btrfs quota enable --simple` on it.
   It must be *dedicated*, because quotas apply to the whole filesystem, and
   the worker refuses a path that isn't the root of one.
2. **A block device**: a zvol, a partition, a disk. Mounted directly, no loop
   device. The worker runs `mkfs.btrfs -m single` on it the first time, and
   refuses to format a device that already has a filesystem it did not create.
   On ZFS this is the recommended setup: `volblocksize=16K`, matching btrfs's
   16K metadata nodes, the same setup that already works for the cargo-target
   zvol. A non-sparse zvol is thick-provisioned, so its space is reserved on
   the host as well. Inside the TrueNAS container it's a `devices:` entry.
3. **An image file**, the default, needing no host setup:
   `<chroot_dir>/pool.img`, loop-mounted. This is the only backing that needs a
   loop device.

In every case the filesystem is `mkfs.btrfs -m single` (the host has its own
redundancy, and DUP metadata would only double metadata writes), mounted
`noatime,compress=zstd:1,commit=120`.

#### Image file details

- `chattr +C` on the directory before the image is created. On a btrfs host,
  without it every write in the pool is copied-on-write a second time on the
  host, and the image fragments badly.
- Sparse (`truncate`), mounted `discard=async`, so space the pool frees goes
  back to the host between builds.
- `losetup --direct-io=on`, falling back to buffered I/O where the host
  refuses it, so data is not cached both inside the pool and on the host. ZFS
  honours `O_DIRECT` from 2.3.
- Inside a privileged container, `/dev` only has the loop nodes that existed
  when the container started. The worker allocates a device through
  `LOOP_CTL_GET_FREE` and `mknod`s `/dev/loopN` if the node is missing. This
  happens once, at startup.
- On ZFS (detected by `statfs` magic), the worker logs once which settings to
  apply to the dataset holding the image:
  - `recordsize=16K`–`32K`. With the default 128K, every 4K write from btrfs
    rewrites a whole 128K record. This is the biggest win.
  - `primarycache=metadata`, so data is not cached in both the page cache and
    ARC.
  - `logbias=throughput`, so the flushes btrfs sends at each commit don't write
    the data twice through the ZIL.
  - Compression: leave ZFS on `lz4`, which gives up quickly on data btrfs has
    already compressed.

  `sync=disabled` would remove the flush cost entirely, and ZFS's ordered
  transaction groups would roll btrfs back to a consistent state on a crash.
  But it has already been ruled out for the cargo-target zvol because btrfs
  needs honoured flushes, so it is not recommended here, even though the pool
  can be recreated.

#### Reservation is optional

A sparse image takes host space only as the builds write. The risk is the host
filling up for some other reason: writes into the pool then fail at the loop
device, btrfs aborts its transaction and goes read-only, and the running builds
fail. That is contained. btrfs is copy-on-write, so the last committed state,
including the base chroot, survives. The pool is remounted, and recreated if it
won't mount, since everything in it can be rebuilt.

To make that unlikely, the worker checks before claiming a job that the host's
free space covers `WORKER_BUILD_DISK_MAX`. That is a guard, not a guarantee.
Operators who want the guarantee use a thick zvol (option 2), or set
`WORKER_DISK_RESERVE=true` on an image: `fallocate` the whole image and
mount without `discard`, so freed space stays in the image for the next build.
On ZFS `fallocate` reserves nothing; `refreservation` on the dataset is the
equivalent, set on the host.

### Chroots become snapshots in the pool

With the base chroot in the pool, a build's chroot is a snapshot of it:

```
<pool>/root              base chroot (subvolume)
<pool>/job-<id>          btrfs subvolume snapshot of root   ┐ qgroup 1/<id>
<pool>/job-<id>.data     work/ (source, PKGDEST)            ┘ limit WORKER_BUILD_DISK_MAX
```

- The worker takes the snapshot and sets up its qgroups before `makechrootpkg`
  runs. `makechrootpkg -r <pool> -l job-<id>` then finds the copy already
  there, the way it finds the overlay today. There is no `-c`, so devtools
  never makes or deletes the copy itself.
- Teardown is `btrfs subvolume delete` for both subvolumes and
  `qgroup destroy`. The command returns at once and the btrfs cleaner frees the
  space in the background, instead of an `rm -rf` over millions of files.
- `run_job` moves the job's workdir (`data_dir/work/<id>`) into
  `job-<id>.data`, so the build's quota covers it too.
- The job's private pacman cache (`Cache::pacman_pkg_job`) becomes a directory
  *inside* the `pacman-pkg` subvolume (`pacman-pkg/.jobs/job-<id>`), not part
  of the build's subvolumes. Promotion stays a plain `rename` within one
  subvolume: atomic and instant, as `promote_job_pkgs` requires. A reflink
  across subvolumes would work mechanically, but simple quotas never move an
  extent's charge: the promoted package would stay charged to the deleted
  build for as long as the cache kept it (measured; see below). The cost of
  this layout is that a build's dependency downloads count against the
  package cache's limit rather than its own. That limit is still hard.

This makes the overlay machinery in `chroots.rs` unnecessary: update layers,
flattening, `MAX_LAYERS`/`HARD_MAX_LAYERS` and the draining that comes with
them, and a `root.lock` held shared for a build's whole duration. They exist
because a live overlay's lower layer must not change. A snapshot is
point-in-time, so the base can be refreshed in place while builds run, and the
exclusive lock is only needed against taking a snapshot.
`design/implemented/overlay-chroot.md` rejected "a new base per refresh" because
a copy costs a full write on the filesystems it targeted. Inside the pool, a
copy is a snapshot on every worker, so that objection no longer applies.

So `Strategy`, `ChrootMode` (`WORKER_CHROOT_OVERLAY`), overlay detection and
layers all go. The retired key is ignored, the same way `RETIRED_SETTING_KEYS`
handles server settings. The base moves into the pool: the first start after
the upgrade builds it there, the same way it's built when missing today, and the
old base under the chroot directory is removed.

### Everything else the worker writes

Builds are only part of the worker's disk use. The rest moves into the pool too:

```
<pool>/root                        base chroot                      qgroup 2/0  WORKER_DISK_MAX
<pool>/job-<id>, job-<id>.data     one build (above)                  ├ 1/<id>   WORKER_BUILD_DISK_MAX
<pool>/srcdest/<pkgbase>           a subvolume per pkgbase            ├ 1/1      WORKER_SRCCACHE_MAX_SIZE
<pool>/builddir/<arch>/<pkgbase>   a subvolume per kept tree          ├ 1/2      WORKER_BUILDDIR_MAX_BYTES
<pool>/pacman-pkg                  shared package cache + job caches  ├ 1/3      WORKER_PKGCACHE_MAX_SIZE
<pool>/gnupg                       shared keyring                     └ (under 2/0 only)
<pool>/tmp                         the worker's TMPDIR

Build ids start well above the fixed cache groups (1/1..1/99 reserved).
```

- **SRCDEST and kept build trees**: a build writes into these, and they
  outlive it, so they were the gap the per-build limit left. Squota charges an
  extent to the subvolume it was written into, so these writes count against
  the cache's qgroup rather than the build's. That is still a hard limit, and
  still under the worker's total.
- **Eviction** becomes `subvolume delete` of one pkgbase's tree: instant,
  where today it is an `rm -rf` over up to millions of files (`cache.rs` has
  its own set-aside-then-delete machinery for exactly that).
- **Measuring** becomes a qgroup query, where today `measure_tree` walks the
  tree, falling back to `sudo du` for what the worker can't read.
- The eviction policy itself (LRU, TTLs, pinning in-use pkgbases) is unchanged.
  The size settings now also stand as hard qgroup limits: eviction keeps a
  cache under its limit between builds, and the qgroup stops a build from
  pushing it past the limit during one.
- **A side benefit**: with one subvolume per kept tree, a build can be given
  only *its own* tree at `/build/<pkgbase>`, rather than the whole per-arch
  directory, where it can reach every other package's tree.
- The worker's own `TMPDIR` points into the pool, so nothing it or its tools
  write to a temp directory escapes the limit either.

**What stays outside, on purpose:** the worker's identity (`worker-key.pem`,
certificate, CA), the staged build credential and its ssh key, and the agent
socket directory. They are small and fixed-size, and nothing a build does
changes their size. And they must survive the pool being recreated: if the
pool is rebuilt after a failed shrink, the worker must not come back
un-enrolled. The worker's own log goes to stdout, where journald or Docker's
log driver bounds it; build logs stream to the server.

So after this change the worker's data directory holds a few kilobytes of
identity, and everything that grows is in the pool.

### Settings

All are worker settings, alongside `WORKER_BUILD_MEMORY_MAX` and
`WORKER_TOTAL_BUILD_MEMORY_MAX`, so they can be set in the environment or from
the worker's page. The existing cache size settings keep their names and
meanings, and now also set the caches' qgroup limits.

| Setting | Meaning | Default |
|---|---|---|
| `WORKER_POOL` | a btrfs mount, a block device, or unset for the image | unset |
| `WORKER_BUILD_DISK_MAX` | disk one build may use | `50G` |
| `WORKER_DISK_MAX` | everything the worker stores: base chroot, caches, builds; also the image's size | `200G` |
| `WORKER_DISK_RESERVE` | pre-allocate the image | `false` |

The pool's own size is the total plus a margin (5%, at least 2G), and the total
qgroup is what actually binds. A btrfs filesystem that is really full is the
one state where deleting from it can fail, so the pool never gets there. With a
device or an existing mount, a total larger than the filesystem can hold is
logged as an error: the filesystem is then what binds.

No check that the cache budgets, a build's quota and the base together fit
under the total: the total is the one hard bound, and the cache budgets only
decide what eviction takes first. A build that finds the pool full fails with
"this worker's storage is full".

A per-package limit was considered and left out for now; one per-build limit per
worker is enough to start with. If it comes back, it is a server setting sent
in `JobDescriptor`, capped by the worker's own limit.

### Changing the size

- **Lowering the limits:** takes effect at once, by writing the qgroup limits.
  A running build already over its new limit gets `EDQUOT` on its next write.
  That is the same choice the memory totals made: what the operator asked for
  applies now.
- **Growing the image:** `truncate` (or `fallocate`), `losetup -c`, then
  `btrfs filesystem resize max`, all online.
- **Shrinking the image** (a lower `WORKER_DISK_MAX`):
  1. Evict caches until what is used fits under the new size. Caches are
     the bulk of the pool and the part that can go, and deleting a subvolume
     is instant.
  2. Try `btrfs filesystem resize <new>` online. btrfs relocates chunks out of
     the space being cut off. If that works, `truncate` the image and
     `losetup -c`.
  3. If it fails (running builds hold too much, or relocation errors): the
     worker drains. It stops claiming jobs, lets the running builds finish,
     unmounts, deletes the image, and creates a new one at the new size. The
     caches start cold and the base chroot is rebuilt; identity lives outside
     the pool, so the worker stays enrolled. While it waits, the worker's page
     shows the drain as its waiting reason.
- **Shrinking a block device pool** (zvol, partition) takes two steps, and the
  order matters.
  1. The worker shrinks the filesystem online to the new total plus margin
     (`btrfs filesystem resize`). If that fails, it falls back to the same
     drain-and-recreate as for an image, with `mkfs` on the device instead of
     a new file.
  2. The operator shrinks the device: `zfs set volsize=<size>` (a multiple of
     `volblocksize`; the TrueNAS UI may refuse to shrink a zvol, the CLI
     doesn't), or `parted resizepart`. The worker can't do that from inside a
     container, so it logs the exact minimum size to use.

  ZFS does not check what lies past a new `volsize`; it simply discards it.
  Shrinking the device before the filesystem destroys the pool, and btrfs
  refuses to mount a filesystem larger than its device. So at startup the
  worker checks that the filesystem fits its device, and refuses with a
  message naming both sizes if it doesn't. After the device has shrunk,
  `btrfs filesystem resize max` takes back any slack left by rounding.
- An existing btrfs mount is the operator's filesystem. The worker only lowers
  the qgroup limit there, and resizing it is up to the operator.

### Failures and reporting

- A worker that cannot set up its pool (no btrfs support, no loop device, a
  device it refuses to format) refuses builds and says why at startup. This is
  the rule the cgroup limits already follow: a configured limit is enforced, or
  nothing runs.
- When a build fails and its qgroup is at its limit, the report says so in the
  terms of the setting to change, next to `oom_reason`:
  - "the build ran out of its 50 GiB disk quota (`WORKER_BUILD_DISK_MAX`)"
  - or, when the parent qgroup is the one full: "this worker's storage is
    full (`WORKER_DISK_MAX`)". Before each build, the worker evicts caches
    until the build's limit fits under the total, so this should mean several
    concurrent builds together filled it.
- The build qgroup's usage at the end of the build is reported beside
  `peak_memory`.

### Where it lives: a crate, shaped like a service

The pool and chroot lifecycle becomes its own crate, `aurcache-chroot`. It takes
over `chroots.rs`, the base-chroot half of `chroot.rs`, and the new pool code.
Its surface is small:

```rust
Pool::open(PoolConfig) -> Pool          // mount or create, enable quotas, sweep leftovers
pool.refresh(&BaseConfig)                // bring root up to date
pool.lease(build_id, Limits) -> Lease    // snapshot + data subvolume + qgroups
lease.root() / lease.data()              // what makechrootpkg and the job need
lease.usage() -> Usage                   // qgroup figures, for reporting
lease.release()                          // delete + qgroup destroy
pool.resize(new_total)                   // grow, shrink, or ask the caller to drain
```

A separate crate buys the things that matter here, without the cost of a
second process:

- The root-gated tests (loop images, quotas, snapshots) run against the crate
  alone, with no server, client or job around them.
- The worker's `job.rs` stops knowing how a chroot is made. Today it reaches
  into leases, overlay strategies and devtools' copy flags.
- The invariants (never format a device we didn't create, never delete a
  subvolume outside the pool, the margin under the total) are enforced in one
  place with a narrow API around them.

**A separate privileged service is not worth it yet.** Its appeal is privilege
separation: the network-facing worker would stop being root-equivalent. But the
worker runs with `NOPASSWD: ALL` (`packaging/aurcache-worker.sudoers`), and that
file explains why: `makechrootpkg` itself is root-equivalent through its
arguments (`-r`, `-d host:chroot`, `-I`). A daemon that only hands out mounted
chroots leaves that call in the worker, so the worker still needs full sudo and
the daemon adds IPC, versioning, a second process in every image and its
supervision, for no security gain.

The boundary that *would* pay for itself is one level up: a local build
executor that owns the pool **and** runs the build. The worker hands it a job
directory and a closed set of options (never a path or a bind), and gets back a
log stream, an exit status and the artifacts. Then the worker could drop sudo
entirely, which is what the sudoers comment says real narrowing requires. That
is a larger project: cgroups, the build firewall, the ssh agent and credential
staging would all move across it too. It deserves its own design.

To keep that option open, the crate's API takes the same shape an IPC protocol
would:
- requests are plain data (build id, sizes, a pacman.conf), never caller-chosen
  paths;
- the crate decides every path itself;
- nothing returned depends on sharing memory with the caller.

Moving it behind a socket later would then be a transport change, not a
redesign.

### The docker worker

`aurcache-worker-docker` stays uncapped, as builds were before workers existed.
It is only used in the hybrid image's legacy socket mode (`BUILD_ARTIFACT_DIR`
set). Its builds write to the container's writable layer, which Docker can only
cap on overlay2 over XFS with `pquota` or on the btrfs/zfs storage drivers, not
on TrueNAS's overlay2 on ZFS. They also write to a host bind directory the
worker can only reach through the Docker socket.

A disk quota is not a property of a build that every worker promises; it is how
the chroot worker keeps its own host safe. So the docker worker says nothing
about it. Whether it can have one at all is for later.

The TrueNAS `builder` service and the hybrid image's default mode both run the
chroot worker, so both get quotas.

## Alternatives rejected

- **A free-space watchdog.** `fallocate`; see above.
- **Project quotas on the host (XFS, ext4 `prjquota`).** Neither host here has
  them: the local worker is btrfs, the TrueNAS one is ZFS.
- **One ext4 image per build.** A hard per-build limit, but no total. Sparse
  images also overcommit the host with no bound on how far.
- **Per-build images inside a shared pool image.** When the pool fills, the
  inner filesystem sees I/O errors at writeback instead of `ENOSPC`, so builds
  die with confusing errors. Pre-allocating each inner image avoids that, but
  reserves every build's whole limit at once.
- **An ext4 pool with project quotas.** Works everywhere and needs only one
  loop device, but teardown is an `rm -rf` of the build's tree again. It also
  keeps the overlay and its layers, where snapshots make them unnecessary.
- **Qgroups on the host's own btrfs.** Only one of the two workers has btrfs,
  and enabling quotas on a filesystem that isn't dedicated affects everything
  else on it.
- **Docker `storage_opt` for the docker worker.** Unsupported on overlay2 over
  ZFS, and the bind directory would remain uncapped.

## Prototype results (2026-09-24)

A root prototype on the local worker (kernel 7.2.4, btrfs-progs 7.1): a sparse
4G image in a `+C` directory on the chroot btrfs, `losetup --direct-io=on`
(which the host accepted), `mkfs.btrfs -m single`, simple quotas. A 300M base
with 5000 files stood in for the chroot.

| Check | Result |
|---|---|
| Fresh snapshot of the base | charged 32 KiB; snapshot + data subvolume in 55-80 ms |
| `fallocate -l 300M` past a 200M build limit | `EDQUOT` at once, nothing allocated |
| `dd` 300M of random data | stopped at 200 MiB, the group at 199M |
| 8K files into the data subvolume | refused after 24127 files, at 199M: both subvolumes share the build's limit |
| Overwriting a base file in the snapshot | charged to the build (100M written, 100M charged) |
| Total `2/0` over three builds and the base | held at 999M of 1000M; the third build refused at 247M of its 400M |
| Writes through a bind mount into a cache subvolume | charged to the cache's group, not the build's |
| `rename(2)` across subvolumes | `EXDEV`, as assumed; within one subvolume, fine |
| Reflink across subvolumes | works, but the charge stays with the writer, even after it is deleted (a `<squota space holder>` until the extent is freed, then `<stale>`) |
| `subvolume delete` | returns in 50-66 ms; the cleaner freed ~450M in ~40 s, and the total dropped accordingly |
| Online shrink 4G -> 2G right after deletes | worked; `truncate` + `losetup -c` + `resize max` too |
| Host blocks used by the sparse image after deletes | 1128M of 2048M (`discard=async` hands space back) |
| A sparse 4G pool on a 1G host filesystem, 1.5G written | the writer gets `EIO` (write or `fsync`); reading back gives `EIO`, never wrong bytes; btrfs aborts the transaction and forces the pool read-only |
| Remount after that | clean; the base chroot's committed data intact; scrub finds no errors |

What this changed in the design:
- The qgroup levels are inverted: the total is level 2, builds and caches
  level 1.
- The job's pacman cache moved into the `pacman-pkg` subvolume, because
  reflinks don't move charges.
- A deleted build's qgroups can't always be destroyed at once (`EBUSY` while
  a space holder still carries charge). The sweep clears them later with
  `btrfs qgroup clear-stale` and destroys empty level-1 groups, instead of
  treating a failed destroy as an error.

So a host filling up underneath a sparse pool is contained the way
"Reservation is optional" assumes: the builds running at that moment fail
loudly, and the pool needs a remount (the worker unmounts and remounts it
when it finds it read-only), not a rebuild.

Item 6 (real build times) from the list below was never measured.

## What the prototype set out to confirm

A root prototype, in a scratch image outside `/tmp`:

1. With simple quotas, a limit on a snapshot of a populated base returns
   `EDQUOT` for `fallocate`, `dd`, and many small files, and nothing gets past
   it.
2. A parent qgroup's limit is enforced under simple quotas. That is what the
   total and the two-subvolume per-build group rely on. If it isn't, fall back
   to a limit on each subvolume and the pool's size as the total.
3. A snapshot is charged only for its own writes. Snapshot creation and
   deletion stay fast. Usage comes back after delete plus `subvolume sync`.
4. Online shrink with `btrfs filesystem resize` works on a pool with deleted
   subvolumes still being cleaned.
5. A sparse image on a host filesystem that fills up: btrfs goes read-only,
   the last commit survives, and it remounts cleanly.
6. Build time for a real package: a pool on the dedicated btrfs mount, and a
   `+C` image on btrfs with and without `--direct-io`, against today's.
7. A build writing into a bound `srcdest/<pkgbase>` subvolume is charged to
   that subvolume's qgroup, not to the build's. A reflink from a job's
   subvolume into `pacman-pkg` works across the subvolume boundary, and the
   extent's charge moves or is shared the way this design assumes.

## Out of scope

Both items first listed here were fixed alongside this work (see "What
shipped"): the archiver following symlinks, and a timeout and output cap on
the server parse.
