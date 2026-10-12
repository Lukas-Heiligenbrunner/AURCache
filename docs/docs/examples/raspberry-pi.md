---
sidebar_position: 2
---

# Raspberry Pi server with an x86_64 worker

The server is cheap to run — it serves the UI, the API and the repository, and
builds nothing — so it fits comfortably on a Raspberry Pi. The builds happen on
a separate x86_64 machine, natively, with no emulation involved. The Pi runs
Raspberry Pi OS (Debian-based, not Arch), so both sides run in Docker.

The result is two architectures from two machines: the Pi serves the
repository, the x86_64 box builds `x86_64` packages for it. An optional local
worker on the Pi adds native `aarch64` builds.

## Server on the Pi

Generate a server-only file — no local worker — and run it on the Pi:

```bash
aurcli setup compose --role backend --database postgres -o - > docker-compose.yaml
docker compose up -d
```

`backend` is the server without a builder; a `bundle` file would also start a
worker on the Pi itself (see below). As on any host:

- Set `AURCACHE_PUBLIC_URL` to how the worker and pacman clients reach the Pi,
  e.g. `http://192.168.1.10:8081`. The default `http://localhost:8081` only
  works from the Pi itself.
- Publish ports `8080` (web UI + API), `8081` (pacman repository) and `8083`
  (worker protocol, HTTPS + mutual TLS) on the LAN.
- Keep `/app` persisted — database (unless PostgreSQL holds it), repository,
  worker CA, build logs, source cache. PostgreSQL is recommended over SQLite
  for anything kept.

The server image is multi-arch, so the same tag runs on the Pi's `arm64`.

## x86_64 worker on the Arch machine

On the x86_64 host, run the worker image against the Pi:

```bash
docker compose -f compose/docker-compose.remote-worker.yaml up -d
```

after editing three things in that file (see
[its header](https://github.com/Lukas-Heiligenbrunner/AURCache/blob/master/compose/docker-compose.remote-worker.yaml)):

1. `AURCACHE_URL=https://<pi>:8083` — the Pi's worker-protocol port.
2. `AURCACHE_SERVER_CA_FINGERPRINT` — pin the server's CA, read from the
   server's startup log line
   `Worker CA fingerprint (pin this on workers): <sha256>`. Without a pin the
   worker trusts on first use, which is fine on a trusted LAN but not over the
   public internet.
3. Approve **this** worker, picking one:
   - pre-approve its certificate fingerprint on the server (recommended — the
     worker prints its fingerprint on first start, add it to the server's
     `AURCACHE_PREAPPROVED_WORKERS=<fingerprint>:<name>:x86_64`),
   - share an enrollment token (`AURCACHE_ENROLLMENT_TOKEN`, same value on
     both sides), or
   - approve it once on the **Workers** page.

There is no shared `enroll` volume between machines, so trust is explicit where
the bundled setup has it implicit.

`WORKER_ARCHES` defaults to the host architecture, so an x86_64 worker needs no
`WORKER_ARCHES` line at all — it is shown commented in the file for the
foreign-arch case. `WORKER_CONCURRENCY_DEFAULT=2` is a starting point the
server may later override per worker. Builds are already parallel inside
(`MAKEFLAGS=-j$(nproc)`), so raise concurrency for memory and disk headroom,
not core count. The pool defaults to a sparse image file in the worker's
volume, which is fine on ordinary filesystems; on ZFS back it with a zvol
instead — see [Storage pool](../workers/storage-pool.md).

Alternatively, if the x86_64 machine runs Arch Linux, skip Docker and install
the worker natively (`paru -S aurcache-worker` — it lives in the AUR, so use
an AUR helper — enrol with a token from the **Workers** page). See [Native install](../setup/native.md#worker).

## Optionally: native aarch64 builds on the Pi

Add a worker to the Pi's own compose project for `aarch64`, enrolling over the
shared `enroll` volume like the bundled setup's builder:

```yaml
builder:
  image: ghcr.io/lukas-heiligenbrunner/aurcache-worker:latest
  environment:
    - AURCACHE_URL=https://aurcache:8083
    - AURCACHE_ENROLLMENT_DIR=/enroll
    - WORKER_ARCHES=aarch64
    - WORKER_CONCURRENCY_DEFAULT=1
  volumes:
    - enroll:/enroll
    - worker_data:/var/lib/aurcache-worker
  privileged: true
  tmpfs:
    - /run
```

Keep concurrency at 1 — a Pi has neither the memory nor the I/O for parallel
chroot builds — and put `worker_data` on an external SSD rather than the SD
card. `aarch64` jobs then prefer this worker over any emulation; `x86_64` jobs
always go to the x86_64 machine, since a foreign architecture is only ever
emulated when **no** worker builds it natively.

## Using it

One stanza serves every architecture — `$arch` is pacman's own variable and is
left unexpanded:

```bash
aurcli repo config | sudo tee -a /etc/pacman.conf
sudo pacman -Sy
```

Add packages from the **Packages** page or with `aurcli pkg add`. Note that
`pkg add --from-installed` mirrors what `pacman -Qm` reports on the machine it
runs on — run it on the Arch machines, not on the Pi, which has no pacman
database to mirror. If a build does not start, see
[Why is a build not starting](../workers/managing.md#why-is-a-build-not-starting).
