---
sidebar_position: 7
---

# Docker worker

The Docker worker builds each package in a container it creates from a builder
image, against a Docker or Podman socket it is given. It is for hosts that
already run a container engine and have no use for a `devtools` chroot, and it
is the builder the [hybrid image](./hybrid.md) embeds.

It builds in a reused image rather than a clean chroot, so it does not support
the source and package caches; the [chroot worker](./split-chroot.md) does. It
does support an SSH key for private sources: see
[Build credentials](#build-credentials).

## How it builds

For each job the worker prepares a build directory holding the job, its
sources and a copy of the **build agent**, creates a container from the builder
image with the engine's resource limits, streams the container's output into
the build log, and uploads the packages the build leaves in the directory. The
container and the directory are removed afterwards, however the build ended,
and when the worker is told to stop (`SIGTERM`, as `docker stop` and
`systemctl stop` send): it stops its builds, cleans up after them and reports
each to the server as interrupted, so that a new build of the package is
queued.

A build's container is named `aurcache_build_<worker>_<build>`, where
`<worker>` is the start of the worker's fingerprint, so two AURCache instances
can share one Docker daemon without taking each other's containers for their
own.

With `BUILD_ARTIFACT_DIR` set the worker builds against the mounted Docker
socket: the directory must be a path the Docker daemon can bind, with the same
directory visible to the worker. It must be on a filesystem that allows
executing: the build agent is run from it, so a mount with `noexec` fails
every build as its container starts. The worker warns at startup when it sees
one. Without it, the [hybrid image](./hybrid.md)
builds against a Podman it runs inside its own container.

### The build agent

The agent is a small static program, `aurcache-build-agent`, that the worker
brings: it is the build container's entrypoint, and the builder image does not
contain it. The package and the hybrid image install it at
`/usr/lib/aurcache/bin/aurcache-build-agent`; `WORKER_BUILD_AGENT` names another
path. The worker refuses to start without it, and with one that is not
statically linked: it runs in the builder image, for any architecture, where
the worker's own libraries are not. The
[Kubernetes worker](./kubernetes.md) runs the same agent, so a build behaves
alike on both.

Inside the container the agent works in two steps:

1. **As root**, it writes the job's `pacman.conf` and mirrorlist, prepares
   pacman's keyring, upgrades the image (`pacman -Syu`) and adds a build user.
2. **As that user, with no privileges**, it unpacks the sources, fetches the
   PGP keys the PKGBUILD names in `validpgpkeys`, and runs makepkg. When makepkg
   needs dependencies installed it asks the agent, which installs the packages
   named and nothing else. There is no `sudo`. `--install` and `--rmdeps` in
   a package's build flags are left out: the container is discarded when the
   build ends, so there is nothing to install into or to clean up.

A step that fails before makepkg (a mirror that is down, a source archive
that does not unpack) is the worker's problem, not the package's: the build is handed back and the server tries it
again while it has attempts left. A PGP key that cannot be fetched is only
noted in the build log, since a PKGBUILD often lists keys no source of the
version is signed with; a build that needed the key then fails at makepkg's
signature check.

When the build ends, the agent writes a report beside the packages: how makepkg
ended, the build's disk use, the commits of its VCS sources, and its peak
memory and out-of-memory kills, read from the container's own cgroup (cgroup
v2; on a host without it those two are left unknown). That is where the
figures on a build's page come from, against the host's Docker and against the
Podman the hybrid image runs inside itself alike.

### What a build may do

A build container is not privileged. It starts as root with every capability
dropped except the seven the agent's first step needs (`CHOWN`, `DAC_OVERRIDE`,
`FOWNER`, `FSETID`, `SETUID`, `SETGID` and `KILL`), and with
`no-new-privileges`, so nothing in it can gain more. Package code runs as the
unprivileged build user, with none of them. The build directory is the
container's only bind mount, and the Docker socket is never given to a build.

The build user is uid 1001 inside the container. Unless the engine remaps
user ids, that is uid 1001 on the host as well, and a host user with that uid
can signal and inspect a build's processes.

By default a build container shares the worker's own network namespace, so
that the repository address the worker uses means the same thing inside the
build. A build can therefore reach whatever the worker can, the server's worker
port included; that port asks for a client certificate, which a build does not
have. `AURCACHE_BUILDER_NETWORK` puts builds on a Docker network you name
instead. Unlike the Kubernetes worker, nothing here firewalls a build.

`/dev/shm` is as large as the memory limit, or 2 GiB when none is set.

### The builder image

`BUILDER_IMAGE` (default
`ghcr.io/lukas-heiligenbrunner/aurcache-builder-v2:latest`) is pulled before
each build. An image of your own needs pacman, makepkg, gpg and `find`; it needs
neither `sudo` nor a build user, which the agent adds.

### Build credentials

`WORKER_GIT_SSH_KEY` names an SSH key for packages whose sources are in private
repositories, and `WORKER_SSH_KNOWN_HOSTS` the host keys of the servers it is
used with; set both or neither, as the worker refuses to start with a key and
no host keys. The key is in the OpenSSH format `ssh-keygen` writes, without a
passphrase: Ed25519, RSA (signing with SHA-2) or ECDSA. Nothing is generated,
unlike on the chroot worker.

The key never enters a build container. The worker holds it and answers for
it: while makepkg downloads a build's sources, before anything is installed
for the build, ssh in the container reaches the worker through `docker exec`
and asks it to sign the login. Then the link is closed for good, and only then
are the build's dependencies installed and its functions run, so a
dependency's install script, which runs as root in the container, finds
neither the key nor a way to use it. A package that clones over SSH in
`prepare()` or `build()` instead of through `source=` therefore cannot
authenticate here, where the [chroot worker](./split-chroot.md) allows it: the
container workers close the window before that code runs.

While the window is open, code in it can sign a login to any host with the
key, though it can never read the key and the worker signs nothing but an SSH
login (no commit signature, no SSHSIG). Give the worker a deploy key that
reaches only what its packages need, read-only where the host allows it, and
use [routing](./routing.md) to send those packages here.

:::note Upgrading
The Docker worker ignored `WORKER_GIT_SSH_KEY` before; it now acts on it, and
a worker with the key set refuses to start without `WORKER_SSH_KNOWN_HOSTS`,
or with a key in the old PEM format (convert it with
`ssh-keygen -p -N '' -f <key>`). The same is true of the embedded worker in
the [hybrid image](./hybrid.md).
:::

### Build trees are not kept

Every build starts from a fresh tree in a new container, so the
persistent build directory setting
(`persistent_builddir`) has no effect on this worker, and nothing is logged
about it. Kept failed builds are not offered either.

## Settings

Its tunables are `CPU_LIMIT` (milli-CPUs) and `MEMORY_LIMIT` (MB, negative for
unlimited). Like the chroot worker's settings they are listed on the worker's
page and can be set there, and read `CPU_LIMIT_DEFAULT` and
`MEMORY_LIMIT_DEFAULT` as defaults AURCache may override; the plain names still
pin. Its builder image and build directory are not settable from AURCache.

The **Type** column on the Workers page tells the strategies apart: `chroot`
for the chroot worker, `docker` for this one — see
[Managing workers](./managing.md#build-strategies).

## Other architectures

It builds for every platform AURCache supports: each build container is
created from the builder image for the package's platform, and a foreign one
runs through the host's qemu/binfmt handlers. Without a handler for a
platform, its builds fail; register one
(`docker run --privileged --rm tonistiigi/binfmt --install all`) or set
`WORKER_ARCHES` to the platforms this host can build, comma separated
(`x86_64`, `aarch64`, `armv7h`).

It competes for those builds like any worker that builds them, so give a
faster one a higher [priority](./routing.md), or pause this one, to keep it
from taking them.

A [chroot worker](./split-chroot.md) builds for its own architecture only, and
for another as the worker image run for that platform.
