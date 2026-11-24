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
container and the directory are removed afterwards, however the build ended.

When the worker is told to stop (`SIGTERM` or `SIGINT`), it claims nothing
more, stops the builds it holds and reports them as interrupted, so the server
queues them again at once. It gives that 30 seconds, and exits at once on a
second signal. `docker stop` kills after 10 seconds unless told otherwise, so
give a container of your own `--stop-timeout 40` (`stop_grace_period: 40s` in
a compose file); `systemctl stop` waits 90. A build that is not reported in time
is queued again once the server stops hearing of it, but its container may
still run: remove it by hand (`docker ps -a --filter name=aurcache_build_`).
The [hybrid image](./hybrid.md) stops its server alongside the worker, so its
builds are usually queued again only that way.

A build's container is named `aurcache_build_<worker>_<build>`, and its
directory under `BUILD_ARTIFACT_DIR` is `<worker>_<build>`, where `<worker>`
is the start of the worker's fingerprint. So two AURCache instances can share
one Docker daemon, and one build directory, without taking each other's
containers or directories for their own. A worker that enrolls again under a
new identity (its data volume lost, say) has a new fingerprint, and does not
clear what a build of its old identity left behind; remove those by hand.

A build first syncs every repository, AURCache's included, then upgrades the
container from the official ones alone -- or those the `pacman_conf` setting
lists -- and only then installs its dependencies, which may come from
AURCache. An upgrade that could see AURCache's repository would act on the
`replaces=` of a package built there, and swap it in for any package in the
image it names that the official repositories no longer carry.

The agent writes that `pacman.conf` as root's own, and root's pacman reads it
after package code has run, to install the build's dependencies. So it must
not point root's pacman at paths a build can write: no `Include`, `CacheDir`,
`HookDir` or `file://` server under `/tmp`, `/var/tmp`, `/dev/shm` or the
build directory. The default one includes its mirrorlists from
`/etc/pacman.d`, which is root's.

With `BUILD_ARTIFACT_DIR` set the worker builds against the mounted Docker
socket: the directory must be a path the Docker daemon can bind, with the same
directory visible to the worker. It must be on a filesystem that allows
executing: the build agent is run from it, so a mount with `noexec` fails
every build as its container starts. Without it, the [hybrid image](./hybrid.md)
builds against a Podman it runs inside its own container.

The worker creates the directory mode `0700` and warns at startup when it is
open to others: keep it `0700`, owned by the worker's user
([why](../overview/upgrading.md#from-0-6)).

### The build agent

The agent is a small static program, `aurcache-build-agent`, that the worker
brings: it is the build container's entrypoint, and the builder image does not
contain it. The package and the hybrid image install it at
`/usr/lib/aurcache/bin/aurcache-build-agent`; `WORKER_BUILD_AGENT` names another
path. The worker refuses to start without it. It must be statically linked:
it runs in the builder image, for any architecture, where the worker's own
libraries are not, and one that is not fails every build as its container
starts. The [Kubernetes worker](./kubernetes.md) runs the same agent, so a
build behaves alike on both.

Inside the container the agent works in four steps:

1. **As root**, it writes the job's `pacman.conf` and mirrorlist, prepares
   pacman's keyring, upgrades the image and adds a build user. The upgrade
   is `pacman -Sy` with every repository the build installs from, then
   `pacman -Su` with the official repositories and your own, but without
   AURCache's: a package AURCache built is installed into a build only when
   that build depends on it. Whatever this step started is ended before the
   next.
2. **As that user, with no privileges**, it unpacks the sources, fetches the
   PGP keys the PKGBUILD names in `validpgpkeys`, and reads the PKGBUILD's
   dependencies (`makepkg --printsrcinfo`).
3. **As root again**, once nothing of that user's is left running, it
   installs those the image lacks, as the chroot worker does: the
   `depends`, `makedepends` and, unless the package builds with
   `--nocheck`, `checkdepends` of the package base, for the build's
   architecture; none under `--nodeps`. Their install scripts run as root
   here, before the build, and whatever they leave running is ended.
4. **As the build user, for good**, it runs makepkg, which checks the
   dependencies and installs none. There is no `sudo`, and nothing in the
   container that would do anything as root for the build.

A step that fails before makepkg (a mirror that is down, a source archive
that does not unpack) is the worker's problem, not the package's: the build
is handed back and the server tries it again while it has attempts left. A dependency that cannot be read or
installed is the package's: the build fails, as it would on the chroot
worker. A PGP key that cannot be fetched is only
noted in the build log, since a PKGBUILD often lists keys no source of the
version is signed with; a build that needed the key then fails at makepkg's
signature check.

When the build ends, the agent writes a report into `out/` in the build
directory: how makepkg ended, the build's disk use, the commits of its VCS
sources, and its peak memory and out-of-memory kills. Those two are read from
the container's own cgroup (cgroup v2; on a host without it they are left
unknown), so they cover the whole container, the image's upgrade and the
dependencies' install included. That is where the figures on a build's page
come from. An out-of-memory failure says so and names `MEMORY_LIMIT`.

A build with no report fails if the engine says the container was killed for
memory. Otherwise it is handed back as the worker's, and queued again while
the package has attempts left. That is what happens when a build fills the
filesystem that holds `BUILD_ARTIFACT_DIR`, or when root in the build (a
dependency's install script) ends it early: it can keep the report from being
written, but cannot forge one. Nothing limits a build's disk use on this worker, so leave room
there for the largest.

### What a build may do

A build container is not privileged. It starts as root with every capability
dropped except the seven the agent's first step needs (`CHOWN`, `DAC_OVERRIDE`,
`FOWNER`, `FSETID`, `SETUID`, `SETGID` and `KILL`), and with
`no-new-privileges`, so nothing in it can gain more. Package code runs as the
unprivileged build user, with none of them. The build directory is the
container's only bind mount, and the Docker socket is never given to a build.
The worker turns off the builder image's health check: each run of it would
be one more process started in the container from outside, which the agent
kills like any other.

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
no host keys. The key is any private key `ssh-add` loads, without a
passphrase, since there is no one to enter it. Nothing is generated, unlike
on the chroot worker.

It is the worker's default key, which a build is offered unless its package's
**Build SSH key** names another. The Docker worker holds no [named
keys](./credentials.md#named-keys), so a build whose package names one is
offered no key at all, and its log says so.

**The key never enters a build container.** For each build, the worker loads
it into an OpenSSH `ssh-agent` of its own, beside the worker and not in the
container, and ssh in the container reaches that agent through `docker exec`.
The build has that while makepkg runs (not while its dependencies are being
worked out and installed), so a package that clones over SSH in `prepare()`
or `build()`, and not only through `source=`, works here as it does on the
[chroot worker](./split-chroot.md). The worker needs OpenSSH for
this: the [hybrid image](./hybrid.md) has it, and beside a worker installed
from the package you install `openssh`.

**A build cannot take the key away, but it can use it while it runs.** The
key is in no file, variable or process of the container, and nothing that a
build can ask the agent for returns it. But `ssh-agent` signs whatever it is
asked to, so the PKGBUILD, or anything that a dependency's install script
(which runs as root before the build) left for the build to run, can:

- log in as the key to any host that accepts it, also for a connection opened
  somewhere else, which stays open after the build ends;
- sign anything else with it, such as a git commit or a file
  (`ssh-keygen -Y sign`).

The [threat model](https://github.com/Lukas-Heiligenbrunner/AURCache/blob/master/design/in-progress/container-workers.md#the-ssh-link)
has the rest. So:

- **Use a key made for builds, and for nothing else**: a deploy key that
  reaches only what the packages need, read-only where the host allows it.
  Use [routing](./routing.md) to send those packages to this worker.
- **Do not register that key as a signing key anywhere**, on GitHub, GitLab
  or in an `allowed_signers` file. Where nobody trusts the key's signatures,
  a signature that a build made with it is worth nothing.

**Which hosts it trusts.** The ssh that git runs in the build
(`GIT_SSH_COMMAND`) trusts the hosts that `WORKER_SSH_KNOWN_HOSTS` names and
no others, whatever the image's `ssh_config` says. That pins git's ssh, and
nothing more: a PKGBUILD can run `ssh` itself without these options, and a
dependency's install script could replace `ssh`. Since a build can log in as
the key anywhere already, that adds no reach; a fetch made that way is just
not checked against your host keys.

A build fetches such sources with `git` and `ssh`, which the builder image
has. A `BUILDER_IMAGE` of your own needs `openssh` too, or every build that
is given the key fails, saying so.

:::note Upgrading
The Docker worker ignored `WORKER_GIT_SSH_KEY` before; it now acts on it, and
a worker with the key set refuses to start without `WORKER_SSH_KNOWN_HOSTS`,
without OpenSSH installed beside it, or with a key protected by a passphrase
(remove it with `ssh-keygen -p -N '' -f <key>`). The same is true of the
embedded worker in the [hybrid image](./hybrid.md).
:::

### Build trees are not kept

Every build starts from a fresh tree in a new container, so the
persistent build directory setting
(`persistent_builddir`) has no effect on this worker, and nothing is logged
about it. Kept failed builds are not offered either.

## Running it by itself

Outside the hybrid image the worker is one program, `aurcache-worker-docker`,
from the AUR package of that name. The package installs the program, the
build agent and the `aurcache` user and directories, and no service unit:
start it from a unit of your own, as a user that may use the engine's socket
(`/var/run/docker.sock`, or the Unix socket `DOCKER_HOST` names). There is no
image of it alone. It removes each build's directory with `rm`, from
coreutils, as its own user. What that leaves, usually what root in the build
made its own, a container of the builder image empties as root, and `rm`
then runs again.

It connects to AURCache and is approved like the chroot worker, with the same
variables: [Connecting to AURCache](./split-chroot.md#connecting-to-aurcache),
[Identity and capabilities](./split-chroot.md#identity-and-capabilities), and
`WORKER_DATA_DIR` for its key. These are its own:

| Variable | Description | Default |
|---|---|---|
| `BUILD_ARTIFACT_DIR` | The build directory as the engine's daemon sees it: the path it binds into each build container. The worker does not start without it | none |
| `BUILD_ARTIFACT_DIR_LOCAL` | The same directory as the worker sees it | `builds` under the worker's working directory |
| `BUILDER_IMAGE` | The image builds run in | `ghcr.io/lukas-heiligenbrunner/aurcache-builder-v2:latest` |
| `AURCACHE_BUILDER_NETWORK` | The Docker network build containers join, or `host` | the network namespace of the worker's own container |
| `WORKER_BUILD_AGENT` | The build agent the worker copies into each build | `/usr/lib/aurcache/bin/aurcache-build-agent` |
| `WORKER_GIT_SSH_KEY`, `WORKER_SSH_KNOWN_HOSTS` | An SSH key for private sources, and the host keys it is used with: see [Build credentials](#build-credentials) | none |

Set both directory variables, to the same path when the worker and the daemon
run on one host. `BUILD_ARTIFACT_DIR_LOCAL` does not default to
`BUILD_ARTIFACT_DIR`: left unset, the worker writes each job under its working
directory while the daemon binds the other path, and every build starts in an
empty directory. In the hybrid image that default is `/app/builds`, which is
where its compose files mount the host's directory.

A build installs its dependencies from AURCache's repository at the address
the worker itself uses, so that address has to mean the same thing inside a
build container. The default arranges that for a worker that runs in a
container. For one that runs on the host, set `AURCACHE_BUILDER_NETWORK=host`,
or name a network on which `AURCACHE_REPO_URL` resolves.

## What builds can reach on the network

The Docker worker puts no network policy on its builds yet; the
[chroot worker](./split-chroot.md#what-builds-can-reach-on-the-network) does.
A build reaches whatever its network does. By default that is the worker's own
network namespace, which in the [hybrid image](./hybrid.md) is the server's: a
build reaches the web UI and API at `localhost:8080`, the worker protocol at
`localhost:8083`, and the worker's local network. Without
[OAuth](../Configuration/authentication.md) the API serves anyone who reaches
it, so a build can add, delete and patch packages and change settings.
Configure OAuth wherever this worker builds packages you did not write, or
build them with a chroot worker.

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

The handler no longer needs the `C` flag. That flag lets qemu run a setuid
program with the program's own credentials, which builds needed for `sudo`. A
build runs nothing setuid now: the agent starts as root, is not emulated
itself, and installs the dependencies before the build. Arch's
`qemu-user-static-binfmt` works as it ships.

It competes for those builds like any worker that builds them, so give a
faster one a higher [priority](./routing.md), or pause this one, to keep it
from taking them.

A [chroot worker](./split-chroot.md) builds for its own architecture only, and
for another as the worker image run for that platform.
