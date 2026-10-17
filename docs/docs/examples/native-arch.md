---
sidebar_position: 3
---

# Native server and worker on one Arch host

Both halves on a single x86_64 Arch Linux machine, no Docker: the server and
one worker as two systemd services sharing the `aurcache` user. Several things
the container images do exist only *because* they are containers and drop away
here — no privileged container, `systemd-nspawn` has a real systemd manager,
and pacman 7's Landlock download sandbox stays on (the images set
`DisableSandbox` for runtimes that block or lack its syscalls; natively those
are normally available).

This page is the worked example; [Native install](../setup/native.md) is the
reference for every setting and for why the worker's unit looks the way it
does.

## Install both packages

```bash
paru -S aurcache-server aurcache-worker
```

Both packages live in the AUR, not the official repositories, so use an AUR
helper (`paru`, `yay`, …).

They are separate because their dependencies are: the server needs no
`devtools`, `base-devel` or `sudo`, and most build machines do not want a
server. The worker install derives its sandboxed `makechrootpkg` from the
`devtools` on this machine (re-derived by a pacman hook on every `devtools`
upgrade) and prints what to do next.

## Server

```bash
$EDITOR /etc/aurcache/server.env
systemctl enable --now aurcache
```

Everything in `server.env` is optional; the defaults serve a single machine on
localhost. Two are worth setting before anything else reaches it:

- `AURCACHE_URL` — how workers and pacman clients address this server, e.g.
  `http://192.168.1.10:8081` once other machines use it.
- The `OAUTH_*` block. With all of them unset there is **no authentication**,
  which is only sensible on a machine nothing else can reach. See
  [Authentication](../Configuration/authentication.md).

The server listens on **8080** (API + web UI), **8081** (pacman repository)
and **8083** (workers). State lives in `/var/lib/aurcache`: the repository,
the database, the worker CA and the build logs.

## Worker

Take an enrolment token from the server's **Workers** page, then:

```bash
$EDITOR /etc/aurcache/worker.env    # AURCACHE_URL and the enrolment token
systemctl enable --now aurcache-worker
```

Minimal `worker.env`:

```bash
AURCACHE_URL=http://localhost:8080
AURCACHE_ENROLLMENT_TOKEN=<token-from-the-workers-page>
```

After the first successful enrolment the worker authenticates with its own
certificate and the token is no longer used. Set
`AURCACHE_SERVER_CA_FINGERPRINT` as well to pin the server's CA instead of
trusting it on first contact.

Concurrency and disk sizing are policy, so prefer the `_DEFAULT` names — a
starting point the server may change from the worker's page — over the plain
names, which pin the value to this machine:

```bash
WORKER_CONCURRENCY_DEFAULT=2
```

Everything the worker stores (base chroot, builds, source and package caches)
lives in its [storage pool](../workers/storage-pool.md), a btrfs filesystem
with quotas. The default is a sparse image at
`/var/lib/aurcache-worker/chroot/pool.img` — nothing to prepare. A zvol, a
spare partition or a dedicated btrfs filesystem can back it instead via
`WORKER_POOL`. On a btrfs root, make the chroot directory a subvolume of its
own before the first start so host snapshots leave the image alone:

```bash
sudo btrfs subvolume create /var/lib/aurcache-worker/chroot
```

## Verify

```bash
systemctl status aurcache aurcache-worker
journalctl -u aurcache-worker -f
```

The **Workers** page should show the worker as connected, with its resolved
settings and where each value came from. Then fill the instance from its own
pacman database and wire the repository in:

```bash
aurcli pkg add --from-installed
aurcli repo config --install
sudo pacman -Sy
```

`pkg add` resolves AUR dependencies, inserts dependency links and enqueues
builds leaf-first; `repo config --install` appends the stanza to
`/etc/pacman.conf` (safe to run twice) and, with a token configured, asks the
server how it actually publishes the repository. If a build does not start,
[`aurcli doctor`](../workers/managing.md#why-is-a-build-not-starting) says
why.

## Growing past one machine

This topology scales by adding workers, not by touching the server: a worker on
another machine enrols with a token and pins the CA fingerprint, and jobs for
its native architecture route to it. See
[Worker Configuration](../workers/split-chroot.md#connecting-to-aurcache)
and the [Raspberry Pi example](./raspberry-pi.md) for the split layout.
