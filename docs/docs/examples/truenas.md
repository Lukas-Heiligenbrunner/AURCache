---
sidebar_position: 1
---

# TrueNAS setup

Server plus one local worker on TrueNAS SCALE, with the worker's
[storage pool](../workers/storage-pool.md) on a zvol instead of the default
image file. The server keeps the database, repository, logs and worker CA; the
worker builds each package in a `devtools` chroot inside that zvol.

## 1. Make the zvol

Create it under your pool, sized at the worker's total (`WORKER_DISK_MAX`)
plus the margin — 5%, at least 2 GiB — or larger, so `WORKER_DISK_MAX` can grow
later. The worker only uses as much of the device as the quota allows.

```bash
zfs create -s -V 1T \
  -o volblocksize=32K \
  -o compression=off \
  -o primarycache=metadata \
  -o logbias=throughput \
  tank/aurcache-worker
```

- `-s` makes it sparse, so it takes pool space as builds write. Leave `-s` out
  to reserve the whole size up front.
- `volblocksize=16K` or `32K`. btrfs's metadata nodes are 16K; 32K trades a
  little write amplification on metadata for less overhead on data. It cannot be
  changed after creation — check it later with
  `zfs get volblocksize tank/aurcache-worker`.
- `compression=off` (or `lz4`): the pool compresses with zstd inside, so ZFS
  finds little left to compress.
- `primarycache=metadata` and `logbias=throughput`: the kernel already caches
  the pool's data, and the flushes btrfs sends need not be written twice
  through the ZIL.

In the web UI this is **Datasets → Add Zvol** with the same size, then set the
three tunables afterwards:

```bash
zfs set compression=off primarycache=metadata logbias=throughput tank/aurcache-worker
```

The device appears on the host as `/dev/zvol/tank/aurcache-worker`.

## 2. Generate the compose file

On any machine with `aurcli` — no server needed:

```bash
aurcli setup compose --role bundle \
  --pool-device /dev/zvol/tank/aurcache-worker --disk-max 950G -o -
```

`-o -` writes to stdout, to paste into TrueNAS as a custom app. Without it the
command writes `docker-compose.yaml` and refuses to clobber an existing file
without `--force`.

What those flags do to the worker service:

- `--pool-device` passes the zvol through as a device
  (`/dev/zvol/tank/aurcache-worker:/dev/aurcache-pool`) and sets
  `WORKER_POOL=/dev/aurcache-pool`. The worker formats it on first start
  (`mkfs.btrfs`, labelled `aurcache-pool`); a device holding a filesystem the
  worker did not create is refused, never formatted.
- `--disk-max` sets `WORKER_DISK_MAX_DEFAULT`, the pool's total — a starting
  point you can later change from the worker's page, not a pin.

The command also asks which database the server should use. PostgreSQL is
recommended for anything you intend to keep (`--database postgres`, with a
generated password unless you pass `--db-password`), and it asks whether to add
the `ixsystems/postgres-upgrade` step (`--postgres-upgrade` /
`--no-postgres-upgrade`) that TrueNAS's own apps use for major-version
upgrades. See [Quick Start](../overview/quick-start.md#for-truenas-portainer-unraid).

## 3. App config

Deploy the pasted file as a custom app. The parts worth checking before it
starts:

- **Device passthrough.** The host path `/dev/zvol/tank/aurcache-worker` must
  resolve on the TrueNAS host; inside the worker it is always `/dev/aurcache-pool`.
- **Server storage.** Everything the server keeps lives under `/app` (the
  database unless PostgreSQL holds it, the package repository, the internal
  worker CA, each build's log, the source cache), persisted in `aurcache_data`.
  Keep that volume on a dataset with room. Splitting pieces onto different
  storage (a large `/app/repo` on bulk disks) works, but the inner mount
  **shadows** whatever the outer volume holds at that path — copy existing data
  across before adding one to a running deployment, or it looks exactly like
  data loss. See [Docker Compose setup](../setup/docker.md#what-to-persist).
- **Ports.** `8080` (web UI + API), `8081` (pacman repository, served by the
  bundled nginx), `8083` (worker protocol). Publish all three on the LAN.
- **`AURCACHE_PUBLIC_URL`.** Set it to how pacman clients reach this machine,
  e.g. `http://192.168.1.10:8081` — the default `http://localhost:8081` only
  works from the host itself.
- **The worker stays `privileged: true` with a tmpfs `/run`.** Each package is
  built in a `systemd-nspawn` chroot, which needs the mounts and namespaces a
  plain container forbids. Only the worker needs this; the server runs
  unprivileged.
- **Enrollment needs nothing.** Server and worker share the `enroll` volume, so
  the worker auto-approves on first start — no token, no approval click.

## 4. Verify

The worker enrolls itself within a few seconds and starts polling for jobs. On
the **Workers** page it should read as connected; if a build does not start,
[`aurcli doctor`](../workers/managing.md#why-is-a-build-not-starting) says
why. Then:

```bash
aurcli pkg add --from-installed
aurcli repo config | sudo tee -a /etc/pacman.conf
```

Moving the pool to another backing later (a larger zvol, a partition) is just
pause, stop, set `WORKER_POOL`, start — the pool holds nothing that cannot be
rebuilt. See [Storage pool](../workers/storage-pool.md#moving-to-another-backing).
