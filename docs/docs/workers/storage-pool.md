---
sidebar_position: 3
---

# Storage pool

Everything a chroot worker stores lives in its **storage pool**: a btrfs
filesystem with quotas that the worker owns. That covers the base chroot, every
running build, and the source and build-tree caches. Its identity is the only
exception. This page covers what can back the pool, how to tune each option,
and how to size it.

| Variable | Type | Description | Default |
|---|---|---|---|
| `WORKER_POOL` | Path | What backs the pool: unset for an image file, or a block device, or the mount point of a dedicated btrfs filesystem | unset |
| `WORKER_DISK_MAX` | Size | Everything the worker stores in its pool: the base chroot, the caches and every running build | `200G` |
| `WORKER_BUILD_DISK_MAX` | Size | Disk one build may write: its chroot, its sources, the packages it makes | `50G` |
| `WORKER_DISK_RESERVE` | Bool | Allocate the whole image up front instead of as builds write (image only) | `false` |
| `WORKER_CHROOT_DIR` | Path | Where the image and the pool's mount point live | `<WORKER_DATA_DIR>/chroot` |

`WORKER_POOL`, `WORKER_DISK_RESERVE` and `WORKER_CHROOT_DIR` describe the
machine, so they are set on the worker only, never from AURCache: they decide
what the worker formats and mounts. The two sizes can also be set from
AURCache (see [`_DEFAULT`](./split-chroot.md#_default-a-value-the-server-may-take-over)).

## How it works

The base chroot is a subvolume in the pool. Each build runs in a snapshot of
it, which takes milliseconds and is charged nothing for what it shares with
the base. Beside the snapshot sits a second subvolume for the build's sources
and packages. Both count against `WORKER_BUILD_DISK_MAX`. Everything together
counts against `WORKER_DISK_MAX`.

The kernel enforces both limits while the build writes. A build that tries to
fill the disk, whether by accident or on purpose (`fallocate -l 1T` is one
syscall), fails with "Disk quota exceeded", and the worker's report says which
limit it hit. Nothing outside the pool is touched. Lowering either value takes
effect at once, including for builds already running. When a build ends, its
snapshot is deleted and btrfs frees the space in the background, so there is
no tree of millions of files to remove.

The filesystem is sized a little above `WORKER_DISK_MAX`: 5% more, and at
least 2 GiB more. The quota is always what binds first, and a completely full
btrfs, where even deleting can fail, never happens.

The pool needs Linux 6.7 or later (btrfs simple quotas) and `btrfs-progs`. A
worker that cannot open its pool takes no builds and says why in its log,
because a build without its quota could fill the host.

## Choosing a backing

| | Image file | Block device | Dedicated btrfs filesystem |
|---|---|---|---|
| `WORKER_POOL` | unset | `/dev/…` | a directory |
| Setup | none | create a zvol or partition | create and mount a btrfs filesystem |
| Loop device | yes | no | no |
| Nested filesystem | yes | no | no |
| Host space | taken as builds write, returned as they are deleted | whatever the device is | whatever the filesystem is |
| Best for | trying it out, any host | ZFS (TrueNAS), a spare disk or partition | a spare partition you want to manage yourself |

In short:

- **ZFS host:** use a zvol.
- **A spare partition or disk:** give it to the worker as a block device.
- **Anything else:** use the image. That includes a host whose only filesystem
  is a btrfs root; see
  [On a btrfs host](#on-a-btrfs-host-a-subvolume-of-its-own).

## An image file (the default)

With `WORKER_POOL` unset, the pool is `<chroot dir>/pool.img`, loop-mounted at
`<chroot dir>/pool`. There is nothing to set up.

- **Sparse.** The image takes host space only as builds write, and gives it
  back as they are deleted. It grows when `WORKER_DISK_MAX` goes up, and
  shrinks when it goes down (see [Resizing](#resizing)).
- **Loop devices.** The image needs one, which a privileged container has. The
  worker creates the `/dev/loopN` node itself if the container lacks it. The
  loop device uses direct I/O where the host filesystem allows it, so pages
  are not cached twice.
- **Where it lives.** `WORKER_CHROOT_DIR` moves the image, and with it
  everything the worker stores apart from its identity, somewhere with room.
  Only the image file itself is visible to the host filesystem. Setuid
  binaries, hardlinks, ownership and file capabilities all live inside the
  image. So an image on NFS or SMB works where a chroot there directly would
  not, though slowly.

### Reserving the space

A sparse image assumes the host will have room when builds need it. If the
host fills up for some other reason, writes into the pool fail, the builds
running at that moment fail with I/O errors, and the pool is remounted. Its
contents are safe, because btrfs only ever commits complete transactions. To
make this unlikely, a worker with a sparse image stops claiming builds while
the host has less than `WORKER_BUILD_DISK_MAX` free, and says so in its log.

If you want a guarantee instead, set `WORKER_DISK_RESERVE=true`. The whole
image is then allocated up front, and freed space stays in the image. On ZFS
this reserves nothing, because ZFS cannot preallocate. Use a thick-provisioned
zvol there, or set `refreservation` on the dataset that holds the image.

### On a btrfs host: a subvolume of its own

The worker marks the image's directory `nodatacow` (`chattr +C`) before it
creates the image, so writes inside the pool are not copied a second time by
the host. `+C` only applies to files created after it is set, so an image made
by an older worker keeps copy-on-write until it is recreated.

Snapshots of the host filesystem, such as snapper's or timeshift's, undo that.
The first write to each block after a snapshot is copied anyway, and the
snapshot pins the image's old contents on disk. Give the chroot directory a
subvolume of its own before the worker first starts. Snapshots never descend
into a nested subvolume:

```bash
sudo btrfs subvolume create /var/lib/aurcache-worker/chroot
sudo chown aurcache: /var/lib/aurcache-worker/chroot   # native install
```

In a container, do the same for the directory you bind to the worker's
`WORKER_CHROOT_DIR`, or for Docker's volume directory.

Why not put the pool directly in a subvolume of the host's btrfs root, with no
image? btrfs quotas are enabled per filesystem, not per subvolume. The worker
would switch the whole root filesystem to simple quotas, which cannot be
turned off without a full rescan, and which snapper's own space accounting
(full qgroups) conflicts with. It would also have to create and delete
subvolumes and quota groups on the host's root. The image costs a loop device
and a nested filesystem, and keeps all of that contained.

### Tuning an image on ZFS

If the image sits on a ZFS dataset, give it a dataset of its own with:

- `recordsize=16K` (or 32K). With the default 128K, every small write from
  btrfs rewrites a whole record.
- `primarycache=metadata`, so data is not cached both by the kernel and in ARC.
- `logbias=throughput`, so the flushes btrfs sends do not write data twice
  through the ZIL.
- `compression=lz4` is fine. btrfs already compresses inside the pool, and lz4
  gives up quickly on data that does not compress.

The worker logs this advice when it finds its image on ZFS. A zvol is still
the better choice there.

## A block device: a zvol or a partition

Set `WORKER_POOL` to the device, for example `/dev/zvol/tank/aurcache-worker`
or `/dev/sdb2`. In a container, pass the device through (`devices:` in compose,
`--device` for `docker run`); `setup` maps it to `/dev/aurcache-pool`.

- **Formatting.** The worker formats the device the first time (`mkfs.btrfs`,
  labelled `aurcache-pool`) and mounts it directly, with no loop device and no
  image. A device that already holds a filesystem the worker did not create is
  refused, never formatted.
- **Mount point.** The device is mounted at `<chroot dir>/pool`.

### Tuning a zvol

```bash
zfs create -s -V 1T \
  -o volblocksize=32K \
  -o compression=off \
  -o primarycache=metadata \
  -o logbias=throughput \
  tank/aurcache-worker
```

- `volblocksize=16K` or `32K`. btrfs's metadata nodes are 16K, and data
  extents are larger. The default (16K on recent OpenZFS) is fine, and 32K
  trades a little write amplification on metadata for less overhead on data.
  It cannot be changed after the zvol is created.
- `compression=off` (or `lz4`). The pool compresses with zstd inside, so ZFS
  finds little left to compress.
- `primarycache=metadata` and `logbias=throughput`, for the same reasons as
  for an image.
- `-s` makes the zvol sparse, so it takes pool space as builds write. Without
  `-s`, the zvol reserves its whole size, which is the guarantee
  `WORKER_DISK_RESERVE` cannot give on ZFS.
- Size it at `WORKER_DISK_MAX` plus the margin (5%, at least 2 GiB), or
  larger. The worker only uses as much of the device as that, so a larger zvol
  leaves room to raise `WORKER_DISK_MAX` later.

On TrueNAS, create the zvol under **Datasets → Add Zvol**, or with
`midclt call pool.dataset.create`, and pass `/dev/zvol/<pool>/<name>` to the
app.

### A partition

Nothing to tune: the worker formats it as it would a zvol. Note that a
partition cannot give space back to anything else while the pool is not using
it.

## A dedicated btrfs filesystem

Set `WORKER_POOL` to the directory where a btrfs filesystem is mounted. In a
container, bind it in; `setup` binds it at `/var/lib/aurcache-pool`. The worker
uses it as it is, with no loop device and no second filesystem. This is the
option to pick when you want to create and mount the filesystem yourself, for
example with your own `mkfs` options or on several devices.

- **It must be a whole filesystem.** It has to be mounted there from its top,
  not as a subvolume of a larger one.
- **It must be dedicated to the worker.** btrfs quotas apply to the entire
  filesystem, so the worker enables simple quotas on it, and a filesystem with
  full quotas already enabled is refused rather than switched.
- **Mount options.** The worker does not remount it. Mount it with the options
  the worker gives its own pools, `noatime,compress=zstd:1,commit=120`, for
  example in `/etc/fstab`:

  ```
  UUID=…  /var/lib/aurcache-pool  btrfs  noatime,compress=zstd:1,commit=120  0 0
  ```

A block device is usually simpler: the worker then formats and mounts it
itself, with the same result.

## Sizing

- `WORKER_BUILD_DISK_MAX` must cover the largest package you build: its chroot
  with its dependencies installed, its sources, and the packages it produces.
  Most builds need a few GB. `unreal-engine` needs around 300&nbsp;GB live.
- `WORKER_DISK_MAX` must cover the base chroot (a few GB), the caches, and the
  builds you want to run at once. Caches are evicted to make room, so they
  shrink as builds grow.

A build's disk use is reported with it, split into chroot, workdir, sources and
build tree, so the build's page shows what a package really takes.

## Resizing

Lowering `WORKER_DISK_MAX` lowers the quota at once. What happens to the
filesystem depends on the backing.

**An image** shrinks with the quota, online, when what the pool holds fits in
the smaller size. When it does not fit:

1. The worker stops taking builds and lets the running ones finish.
2. It recreates the pool, empty, at the new size. Its caches start cold, and
   the next build rebuilds the base chroot.

Its identity lives outside the pool, so the worker stays enrolled. It logs
each step. Raising `WORKER_DISK_MAX` grows the image online.

**A block device or a dedicated filesystem.** The worker fits the btrfs
filesystem to the total, online. It shrinks the filesystem when
`WORKER_DISK_MAX` comes down, and grows it back, as far as the device allows,
when it goes up.

- **It never touches the device itself, and never empties the pool.** When
  what the pool holds does not fit, the filesystem keeps its size, the worker
  logs an error, and the total still binds.
- **Shrinking the device is up to you.** After a shrink, the worker logs the
  smallest size the device can safely go down to. You then resize the device
  yourself: a zvol with `zfs set volsize=`, or a partition with your
  partitioning tool.
- **Never shrink the device below that size, or before the filesystem.** ZFS
  discards whatever lies past the new end, and btrfs refuses to mount a
  filesystem larger than its device.

To grow a device, grow it first (`zfs set volsize=`), then raise
`WORKER_DISK_MAX`.

## Setting it up with the CLI

`aurcli setup worker` and `aurcli setup compose` ask what should
back the pool when they have a terminal. Otherwise they take it as flags:

| Flag | Effect |
|---|---|
| `--pool-image` | An image file in the worker's volume (the default without a terminal) |
| `--disk-reserve` | Allocate the image in full up front |
| `--pool-device PATH` | A block device on the host, passed through as `/dev/aurcache-pool` |
| `--pool-mount PATH` | A dedicated btrfs filesystem mounted on the host, bound at `/var/lib/aurcache-pool` |
| `--disk-max SIZE` | Sets `WORKER_DISK_MAX_DEFAULT`, the pool's total (`500G`, `1T`) |

```bash
# TrueNAS: a compose file for a worker on a zvol
aurcli setup compose --role bundle \
  --pool-device /dev/zvol/tank/aurcache-worker --disk-max 950G -o -
```

## Moving to another backing

The pool holds nothing that cannot be rebuilt: the base chroot and caches. To
move to another backing:

1. Pause the worker with `aurcli worker pause <id> --wait`.
2. Stop it.
3. Set `WORKER_POOL`, and start it again.

The worker creates the new pool, and the next build rebuilds the base chroot.
You can then delete the old `pool.img`.
