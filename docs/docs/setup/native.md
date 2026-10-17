---
sidebar_position: 6
---

# Native install (systemd)

AURCache runs on the host without Docker, as two systemd services. This is
often the better choice for the worker: several things the container images do
exist only *because* they are containers, and drop away here.

- `systemd-nspawn` needs a systemd manager, which a container lacks — the image
  ships a wrapper for it. A real host has one.
- pacman 7's Landlock download sandbox needs syscalls that some runtimes block
  or do not implement (older seccomp profiles, qemu emulation), so the images
  set `DisableSandbox` to work everywhere. Natively those syscalls are normally
  available, so it stays on.
- No privileged container, and no qemu/binfmt for the machine's own
  architecture.

## Packages

```bash
paru -S aurcache-server   # the build server and web UI
paru -S aurcache-worker   # a build machine
```

They live in the AUR, not the official repositories, so install them with an
AUR helper (`paru`, `yay`, …).

They are separate because their dependencies are: the server needs no
`devtools`, `base-devel` or `sudo`, and most build machines do not want a
server. Both may be installed together on one host; they share the `aurcache`
user. See [Native packages](./artifacts/native-packages.md) for what the
packages contain and how to build them yourself.

## Server

```bash
$EDITOR /etc/aurcache/server.env
systemctl enable --now aurcache
```

It listens on **8080** (API and web UI), **8081** (the pacman repository) and
**8083** (workers). State lives in `/var/lib/aurcache`: the repository, the
database, the worker CA and the build logs.

### Build logs

Each build's log is `build_logs/<pkgbase>/<number>.log` under the server's
working directory, overridable with `AURCACHE_BUILD_LOG_PATH` — named after the
build's public identity, the same `<pkgbase>/<number>` the API, the CLI and the
web UI use.

Two things worth knowing:

- **A database backup does not contain build logs.** Back up the log directory
  alongside the database if the logs matter to you.
- **A log whose file is missing is not an error.** The build page shows "no log
  for this build" rather than failing, which is also what you get for a build
  that never produced output.

Every PKGBUILD the server reads is sourced through a sandbox — see [Package
parsing](../Configuration/package-parsing.md). That holds for every install
method, not just this one.

Everything is optional in `server.env`; the defaults work for a single machine.
Two worth setting before anything else reaches it:

- `AURCACHE_URL` — how workers and pacman clients address this server.
- The `OAUTH_*` block. With all of them unset there is **no authentication**,
  which is only sensible on a machine nothing else can reach. See
  [Authentication](../Configuration/authentication.md), and note
  `OAUTH_ALLOWED_USERS` if your provider is a public one.

## Worker

```bash
$EDITOR /etc/aurcache/worker.env    # AURCACHE_URL and an enrolment token
systemctl enable --now aurcache-worker
```

Get the enrolment token from the server's **Workers** page. After the first
successful enrolment the worker authenticates with its own certificate and the
token is no longer used.

### What the package sets up

Two users, and the split between them is the security model rather than
tidiness:

| | |
|---|---|
| `aurcache` | runs the worker process; owns the mTLS identity and build credentials, and is the only user that can read them |
| `builder` | runs each package build, named explicitly by `makechrootpkg -U`, so a build never inherits the worker's user and cannot reach its secrets by file permissions |

`builder`'s **primary** group is `aurbuild`. That is load-bearing, not
incidental: `makechrootpkg` carries only the build user's primary uid/gid into
the chroot, so a supplementary group does not exist in there and group-write on
the caches fails from *inside* the chroot, after every host-side check has
already passed.

Directories, with ownership that matters for the same reason:

```
/var/lib/aurcache-worker           aurcache  worker state
/var/lib/aurcache-worker/secrets   aurcache  0700, build credentials
/var/lib/aurcache-worker/work      builder   2775, shared to the worker by group
/var/lib/aurcache-worker/chroot    aurcache  the storage pool: its image and mount point
```

Everything that grows -- the base chroot, each build, the source and package
caches -- lives in the storage pool, a btrfs filesystem the worker creates in
`chroot/pool.img` and mounts at `chroot/pool`, with a disk quota per build and
one over the whole worker. A zvol, a partition or a dedicated btrfs filesystem
can back it instead of the image, set with `WORKER_POOL`. On a btrfs root, make
`/var/lib/aurcache-worker/chroot` a subvolume of its own before the first start,
so host snapshots leave the image alone. See [Storage pool](../workers/storage-pool.md).

### Why the worker's unit is not hardened

`aurcache.service` takes the full set of systemd hardening directives.
`aurcache-worker.service` deliberately does not, and the unit says so inline.
Its whole job is `sudo mkarchroot` and `sudo makechrootpkg`, which mount,
unshare and chroot. `NoNewPrivileges=yes` alone stops every build before it
starts; `ProtectSystem=strict` makes the chroot and pacman's cache read-only;
`RestrictNamespaces=yes` forbids what `systemd-nspawn` *is*.

The isolation that matters here is not systemd's. Each build runs in its own
devtools chroot as a different unprivileged user, and the two places a PKGBUILD
is executed *outside* that chroot are confined with Landlock.

### The patched `makechrootpkg`

`makechrootpkg` executes the PKGBUILD twice outside the chroot — once for
`makepkg --verifysource`, once to read `pkgbase`/`pkgname`. A PKGBUILD is bash,
so unconfined either one lets a build write another build's files. Checksums are
no defence: they are declared by the PKGBUILD being run.

The package installs a copy at `/usr/lib/aurcache/bin/makechrootpkg` with those
two sites wrapped in `aurcache-sandbox`, and the worker names that path
absolutely. It is derived at install time from the `devtools` on **this**
machine rather than shipped pre-patched, because `makechrootpkg` sources six
files from `/usr/share/devtools/lib` and `/usr/share/makepkg/util` at runtime —
a copy taken against one devtools release would end up running against
another's libraries.

A pacman hook re-derives it whenever `devtools` is installed or upgraded. If a
future devtools moves one of those two call sites the hook **fails the
transaction and removes the stale copy**, so the worker refuses to build rather
than quietly building with two steps unconfined. If that happens, please report
it with your `devtools` version.

Nothing is installed into `/usr/local`, and nothing shadows `makechrootpkg` for
other users: running it yourself gets the packaged one.

## Both on one machine

Install both packages. They share the `aurcache` user and their state
directories are separate, so no extra configuration is needed — point
`AURCACHE_URL` in `worker.env` at `http://localhost:8080`.

## Building the packages yourself

See [Native packages](./artifacts/native-packages.md).
