# Container workers: a build agent, and the Docker worker on it

Status: **Built, not yet merged** (branch `feat/build-agent`) · Last
updated: 2026-11-23 · Spikes run on Docker, kernel 7.2, with the published
`aurcache-builder-v2`

The agent, the worker's side of it and the Docker worker on the agent are
built, and both hybrid suites build through the agent. What "Later" lists was
left out on purpose.

The Docker worker builds each job in a container made from the builder
image, which it asks a Docker (or Podman) daemon for. This is the design of
what runs the build there: the **build agent**, a small program that runs the
build inside that container, and the worker-side code that talks to it,
which is kept apart from the Docker worker so that a worker on another
container engine can use both. It is also where the Docker worker stops
being a compatibility path.

Related:

- `design/proposed/build-runner.md`: the same move for the chroot worker (the
  PKGBUILD only ever runs inside the build's own container, from inputs that
  are data), which has not been made yet.
- `design/proposed/unprivileged-workers.md`: option H (a container per
  build).
- `design/implemented/worker-configuration.md`: what a worker may declare.

---

## Where things stand

### The Docker worker was a way out

`aurcache-worker-docker` reproduces how AURCache built before it had
workers: a container per package from the builder image, against a Docker
socket. It was kept so that deployments from before the split kept building,
called "the legacy container builder", flagged on the Workers page, and
marked deprecated in its own startup log.

It is a supported worker again, under its own name. It is the one worker
that needs nothing of the machine but a container engine: no btrfs, no
Linux 6.7, no privileged container of its own. What made it the lesser
worker was how it built, and that is what changes here.

### How it built

```
bash -lec 'sudo pacman -Syu ... && cd /output/src/<pkgbase> && sudo chmod -R a+w . &&
           <import validpgpkeys> && makepkg --config /output/makepkg.conf -s <flags>'
```

run as the image's `ab` user, which may `sudo pacman`, `pacman-key` and
`chmod` without a password. Three consequences:

- **The build is root whenever it likes.** `sudo pacman -U` of a package it
  just made runs that package's install script as root in the container,
  with the container's default capabilities
  (`unprivileged-workers.md`, "The paths to host root", step 2).
- **The worker cannot write `/etc`.** So `pacman.conf` and the mirrorlist are
  bind-mounted over the packaged files, read-write, because a read-only one
  fails `pacman -Syu` whenever it upgrades the owning package
  (`executor.rs`, `etc_bind`).
- **Foreign architectures need more of the host.** Under qemu, `sudo` only
  works when the binfmt handler is registered with the `C` flag, which Arch's
  own handler package does not set.

---

## Naming

| was | is |
|---|---|
| "legacy container builder", "legacy builder", "compatibility builder" | **Docker worker** |
| `docs/docs/workers/legacy-docker.md` | `docs/docs/workers/docker.md`, with a redirect from the old address |
| `describe_self`: `legacy container (<image>) [deprecated]` | `docker (<image>)` |
| the Workers page highlighting `docker` as deprecated | no highlight |

"Docker worker" is what its `kind` (`docker`), its crate, its binary and its
package already say. "Container worker" would not tell it from the chroot
worker, which also ships as a container image, nor from any other worker
that builds in containers.

What does **not** change, because deployments set them: `BUILDER_IMAGE`,
`BUILD_ARTIFACT_DIR`, `BUILD_ARTIFACT_DIR_LOCAL`, `CPU_LIMIT`, `MEMORY_LIMIT`,
`AURCACHE_BUILDER_NETWORK`, and the `docker` kind, the binary's name and the
package's. The setting keys (`cpu_limit`, `memory_limit`) stay too: the
server stores values under them.

The **hybrid image** (the server and a Docker worker in one container) is a
separate question. It stays described as the compatibility image for compose
files from before the split, because that is what its entrypoint is: mode
inference from old variables, a shared secret, two processes under one
shell. Its documentation stops calling the worker inside it deprecated.

---

## The build agent

`aurcache-build-agent` is one statically linked binary. A worker puts it in
the build container, makes it the container's command, and gives it a
directory. It runs the whole build and leaves a report.

### Why a binary, and why the worker brings it

The alternative is what the Docker worker had: a shell command composed by
the worker. The agent has to do things a command line cannot do safely:
hold root while the build runs without it, install the build's dependencies
as root from what the build user read out of the PKGBUILD, and end every
process the build leaves before root acts again. And any worker that builds
in a container needs exactly the same thing, so it is written once.

**The worker brings its own agent; it is not part of the builder image.**

- The builder image is released on its own schedule (every build of it is
  `latest`), and may be an operator's own (`BUILDER_IMAGE`). An agent baked
  into it would be whatever version that image happened to have. Brought by
  the worker, the agent is always the worker's own build.
- The builder image stays what 0.6.0's worker expects (the `ab` user, its
  sudo rules, an entrypoint that runs its arguments), so an older Docker
  worker keeps working with a newer image. The one thing it gains is
  `openssh` beside `git`, for builds that are given an SSH key.

The Docker worker copies the agent into the build's directory, which is the
one path it shares with the daemon.

**Static**, against glibc, because the container it runs in is not the
worker's: another image, possibly another glibc, possibly another
architecture. Three runs settled it, with a throwaway Rust program that
prints its own capabilities and spawns a child through
`setgroups`/`setresgid`/`setresuid` and `PR_SET_NO_NEW_PRIVS`:

| | run | result |
|---|---|---|
| S1 | built for `x86_64-unknown-linux-gnu` with `-C target-feature=+crt-static`, bind-mounted read-only into `aurcache-builder-v2`, under the flags a build container gets | `static-pie linked`, 1.7 MB; root has `CapEff 0xfb` (the seven), the child is uid 1001 with `CapEff 0`, `NoNewPrivs 1` |
| S2 | the same x86_64 binary in a `--platform linux/arm64` container (`busybox`), on an x86_64 host with `qemu-aarch64` registered | the agent runs natively; the child it spawns reports `aarch64`, with `CapEff 0` and `NoNewPrivs 1` |
| S3 | built for `armv7-unknown-linux-gnueabihf` the same way, with the AUR cross toolchain | links (`statically linked`); not run |

So one statically linked agent serves any builder image, glibc version and
all, and on a host that builds foreign architectures through binfmt the
agent stays native while everything it starts is emulated. Static linking
against glibc needs only `libc.a`, which the toolchains the images already
build with all ship. The agent resolves no user names and no host names,
which is what static glibc cannot do.

**An interface number.** The agent and the worker are the same build when
things are set up right, and a worker can be handed another agent
(`WORKER_BUILD_AGENT`). The spec and the report carry `interface: 1`, and the
agent refuses a spec with any other number, as `aurcache-build` refuses a
worker that expects another interface (`build-runner.md`, step 0).

### The directory it is given

```
<work>/                       given by the worker; the agent takes it for root
  in/spec.json                the BuildSpec: the job, as data
  in/source.tar.gz            the server's source archive, as downloaded
  pkg/                        PKGDEST: what the worker collects
  out/report.json             the BuildReport, written last
<scratch>/                    named in the spec; never read by the worker
  src/<pkgbase>/              the extracted source: makepkg's start directory
  build/                      BUILDDIR
  srcdest/                    SRCDEST
  home/                       the build user's home and keyring
  makepkg.conf                the build's, written by root
  ssh/agent.sock, known_hosts with the worker's SSH key, while makepkg runs
  ssh-link/link.sock          root's: where the worker's `ssh-link` connects
```

The worker reads back `pkg/` and `out/report.json`, and nothing else; the
build's output is the agent's stdout. The scratch directory is the worker's to place: the
Docker worker's is `/var/lib/aurcache-build`, inside the container, so that
nothing owned by the build user is left in a host directory.

### `build`, step by step

As PID 1 and root, with `CHOWN`, `DAC_OVERRIDE`, `FOWNER`, `FSETID`,
`SETUID`, `SETGID` and `KILL` and nothing else, and `no_new_privs` set by
the runtime:

1. **Take the directory.** `chown root:root <work>`, mode 0755, remembering
   who owned it. The build user can then create nothing beside `out/`, and
   cannot replace it: the owner of a directory can rename what is in it, and
   on a Docker host the worker's uid may be the build user's.
2. **Read the spec**, refusing another interface number. It carries two
   pacman configurations: `pacman_conf`, the job's with the AURCache
   repository appended, and `pacman_upgrade_conf`, the job's as the server
   sent it. Write the first as `/etc/pacman.conf`, the second as
   `/etc/aurcache/pacman-upgrade.conf`, and the mirrorlist. Root writes
   them, so nothing is bind-mounted over a packaged file any more, and the
   build user cannot change either. Neither is checked: both are the
   server's `pacman_conf` setting, the admin's own, as on the chroot worker.
   It must not point root's pacman, which reads it again after the build
   user has run, at paths a build can write (`docker.md` says so). With
   `ssh` in the spec, the worker's SSH link is taken before this ("The SSH
   link").
3. **Keyring and upgrade.** `pacman-key --init` (what the image's entrypoint
   did), then the upgrade in two commands:

   1. `pacman -Sy --noconfirm`, with `/etc/pacman.conf`: every database the
      build will install from, AURCache's included. Downloading a database
      runs no code.
   2. `pacman -Su --noconfirm --config /etc/aurcache/pacman-upgrade.conf`:
      the upgrade, which sees the job's own repositories and not AURCache's.

   pacman acts on a package's `replaces=` during an upgrade and at no other
   time, and `--noconfirm` accepts the replacement. An upgrade that could
   see AURCache's repository would let a package built there install itself,
   as root, into every later build on the instance, whether or not that
   build depended on it. pacman loads only the databases a configuration
   lists, so the upgrade's configuration is what keeps them out. There is
   one sync, so the build installs from the databases the image was upgraded
   against. Either command failing fails the build at its `sync` step: an
   upgrade that stopped over fresh databases is a partial one. Nothing after
   this upgrades anything: the dependencies are installed with
   `pacman -S --asdeps`, which ignores `replaces=`.
4. **The build user.** First, whatever the root step left running
   (pacman-key's gpg-agent) is killed, as in step 8, so the build user's
   processes have nothing of root's beside them. Then a line in
   `/etc/passwd` and `/etc/group` for a fixed uid and gid (1001), written
   directly rather than through `useradd`, unless the image already has
   that uid. The scratch directories and `pkg/` are made and given to it,
   and root writes the build's `makepkg.conf` beside them: the image's own,
   the job's assignments, then, last so that they win, `PKGDEST`,
   `BUILDDIR`, `SRCDEST` and the parallelism for the spec's CPU count
   (`aurcache_common::parallelism`, shared with the chroot worker).
5. **As the build user**, each through `setgid`, `setgroups(0)`,
   `setuid` and `PR_SET_NO_NEW_PRIVS`, which leaves a process with no
   capabilities and no way to regain one: unpack the source archive with
   `bsdtar`; import
   the job's `validpgpkeys` from the keyserver into its own keyring (a key
   that cannot be fetched is noted in the log and the build goes on, as on
   the chroot worker); print the PKGBUILD's `.SRCINFO`, with
   `makepkg --printsrcinfo` through the login shell and the `makepkg.conf`
   the build will have, so the PKGBUILD is sourced as the build sources it.
   Its stdout is read back, 16 MiB at most; its stderr goes to the log.
   Under `--nodeps` it is not printed at all.
6. **The dependencies**, as root, once every process of the build user's
   is dead (as in step 8). See below.
7. **makepkg**, as the build user, for good: `makepkg` without `-s`, the
   flags every build takes (`--noconfirm --noprogressbar --nocolor`), then
   the job's, in `src/<pkgbase>`. makepkg checks the dependencies itself,
   and a build missing one fails with makepkg's own message. Its output
   goes to the agent's stdout. With the worker's SSH key, it has the
   agent socket while it runs.
8. **Afterwards.** Kill whatever the build left running: as PID 1,
   `kill(-1, SIGKILL)` reaches every other process in the container. Then,
   as the build user, ask git what each VCS source was at (as the build
   user because git on a repository that the build wrote reads
   configuration that the build chose).
9. **Measure**: `memory.peak` and `memory.events`' `oom_kill` from the
   container's own cgroup, found through `/proc/self/cgroup` so that a
   container sharing the host's cgroup namespace does not read the host's;
   the bytes under the work and scratch directories.
10. **Hand back.** Remove `out/`, which must be the empty directory it was,
   make it again and write `out/report.json` there; then give the work
   directory, and `pkg/` and `out/` with the regular files in them, back to
   whoever owned it, readable by anyone. Last, after step 8, so nothing the
   build started can write the report or change a package. Anything root in
   the container left at `out/`, or a full filesystem, means no report: the
   build reads as interrupted.

The agent exits 0 when it wrote a report, whatever the build did; the build's
own result is in the report. It keeps no signal handler (it resets the
`SIGSEGV` and `SIGBUS` ones that the Rust runtime installs), so the kernel delivers
PID 1 nothing from inside its PID namespace, and only `SIGKILL` from outside:
the worker stops a build by killing the container. The agent is not dumpable
either, so root there, without `CAP_SYS_PTRACE`, cannot trace it. Root in the
container (a dependency's scriptlet) can still end the build early, as
interrupted, but cannot forge its result: lowering the agent's resource limits
(`prlimit --pid 1`, a file size of 0 or a CPU limit) or raising its
`oom_score_adj` leaves no report, as step 9's cases do.

### The dependencies

makepkg installs a build's dependencies itself under `-s`, through
`PACMAN_AUTH` (`sudo` by default), while the build runs. That would need
something of root's to answer the build. The agent installs them before the
build instead, as the chroot worker does (`stage_build_deps`):

- the build user prints the `.SRCINFO` (step 5);
- the agent kills everything the build user has left running, so root's
  pacman and the scriptlets it runs have no process of the build's beside
  them. Its files stay where it left them (see the threat model);
- `aurcache_common::build_deps`, the chroot worker's own rule, reads the
  list from it: the package base's `depends`, `makedepends` and, unless the
  job's flags skip `check()`, `checkdepends`, each with its `_<arch>` array
  for the spec's `arch`, as written (version constraints included), each
  once. A split package's own `depends` are not the build's;
- `pacman -T` says which of those no installed package satisfies, as
  makepkg asks it, so a name an installed package provides does not pull in
  a second provider;
- root installs those with
  `pacman --noconfirm --noprogressbar --color never -S --asdeps --needed -- <targets>`,
  with pacman's output in the log. Either failing fails the build at its
  `dependencies` step.

Once makepkg starts there is nothing in the container that acts as root for
the build: no socket, no `sudo`, no capability. What makepkg
itself would do as root (`-s`, `-r`, `-i`) fails for want of it; the server
already drops `-s` and `-r` from a package's flags and refuses `-i`.

The `.SRCINFO` is the PKGBUILD's output, and the PKGBUILD can print what it
likes. It only decides which targets root's pacman is given: every word
before `--` is the agent's, so whatever the PKGBUILD prints is a name pacman
looks up in the synced databases, and a path, a URL or an option is a name
it does not find. The build can therefore have root install any package
from the job's repositories as a dependency, the AURCache repository
included, as it can on the chroot worker. It cannot have root install a
file, remove a package, or run pacman with other options.

A hostile package already published in one of those repositories runs its
install scriptlet as root in the container of every build that depends on
it, before that build starts. That is true of the chroot worker too. It
runs before the build has the SSH agent, but it can change what the build
later runs, which then has the agent: that is why the SSH key is never in
the container.

What the chroot worker does and the agent does not: it downloads the
dependencies with network and installs them with none (`aurcache-sandbox
offline`), and drops `CAP_MKNOD` from the install's container. A build
container has one network for everything in it, and the build has it too,
so the install runs with that network; and its root has none of
`CAP_MKNOD`, `CAP_SYS_ADMIN` or `CAP_NET_ADMIN` to begin with.

### The report

```rust
pub struct BuildReport {
    pub interface: u32,
    pub ended: Ended,
    // Each optional, and left out of the JSON when unknown.
    pub peak_memory_bytes: Option<u64>,
    pub oom_kills: Option<u64>,
    pub disk_bytes: Option<u64>,
    pub vcs_commits: BTreeMap<String, String>,
}

pub enum Ended {
    /// makepkg ran, and exited with this code.
    Exited { code: i32 },
    /// A signal ended makepkg.
    Signaled { signal: i32 },
    /// A step before makepkg failed, or makepkg could not be started, so
    /// the package was never built.
    Setup { step: SetupStep, message: String },
}
```

A container that ends with no report was killed whole (out of memory, with
`memory.oom.group`; the node; the operator), and the worker hands the build
back as interrupted, with the engine's exit status.

### The SSH link

A build that is given the worker's SSH key reaches it without the key
entering the container. The worker keeps the key in an OpenSSH `ssh-agent`
of its own, one per build: started as the worker's user on a socket in a
private directory, given the key on `ssh-add -`'s standard input, and killed
when the build ends. An agent per build, rather than one for the worker's
life, needs no supervision: an agent that dies takes one build's logins with
it, and the next build starts its own with the key as it is then. A build
reaches it over one exec into the container, which works the same against a
remote Docker daemon and in a Kubernetes pod, where no path can be shared
with the worker.

1. With `ssh` in the spec, the agent first listens on `ssh-link/link.sock`,
   in a directory that only root can enter, before anything of the job runs.
2. As the container starts, the worker execs `aurcache-build-agent ssh-link`
   into it. That process makes itself undumpable, connects, and copies bytes
   between the socket and the exec's stdin and stdout. The agent takes that
   one connection, checks that its peer is root, and both ends exchange a
   hello (`aurcache-ssh-link 1`). A link that has not come within two
   minutes stops the build at its `credential` step, as the worker's
   failure.
3. The kills between steps spare that one process: by its pid, and only
   while it has no parent in the container's PID namespace, as a process
   that the engine started from outside has none. Any other exec is killed,
   so the Docker worker turns the image's health check off.
4. Before makepkg, the agent writes the hosts' keys and opens
   `ssh/agent.sock` for the build user (mode `0600`, in a directory that
   only the build group can enter). `makepkg.conf` exports `SSH_AUTH_SOCK`
   and a `GIT_SSH_COMMAND` with the chroot worker's pinned options
   (`aurcache_common::shell::pinned_ssh_command`).
5. Each request sent there crosses the link as one agent protocol message,
   and its reply comes back, one request at a time. The worker passes on
   `SSH_AGENTC_REQUEST_IDENTITIES` and `SSH_AGENTC_SIGN_REQUEST`, and
   answers every other request with `SSH_AGENT_FAILURE`, so that a build
   cannot add keys, load a provider or call an extension in its agent.
6. When makepkg ends, the agent removes the socket and shuts the link; the
   kill that follows ends `ssh-link` with the rest.

`WORKER_SSH_KNOWN_HOSTS` is required with `WORKER_GIT_SSH_KEY`: a container
is new for every build, so an ssh that accepts the first key that a host
shows would accept any host. The worker loads the key once when it starts,
so that a key that `ssh-add` refuses (one with a passphrase, for one) stops
it there.

| Who, and what | Outcome |
|---|---|
| the build, or root in the container through a dependency's scriptlet, copies the key | not possible: the key is in no file, environment or process of the container, and the agent protocol has no request that returns it |
| root in the container takes over `ssh-link` | not possible: it is undumpable |
| something in the container reaches the link socket first | not possible: the link is taken before anything of the job runs |
| the build logs in as the key while makepkg runs | possible, to any host that accepts it, also for a connection opened elsewhere, which then outlives the build |
| the build signs something else (`ssh-keygen -Y sign`) | possible; such a signature is worth something only where the key is trusted as a signing key |
| the build fetches without the pinned host keys | possible with an `ssh` that the PKGBUILD runs itself, or one that a dependency's scriptlet replaced |
| the build floods the worker's agent | at most 32 connections, and 256 KiB per message |

So the key should be a deploy key made for builds, and not registered as a
signing key anywhere. Destination constraints (`ssh-add -h`) would limit
where a build can log in, but need the link to pass
`session-bind@openssh.com` and to keep one connection to `ssh-agent` per
connection of the build's; they can be added later without changing what a
build sees.

### What else it does

For the Kubernetes worker, which has no shared directory and reaches the pod
only through `exec`, the same binary has the other end of each exchange:
`stage` (install itself into a volume, wait for the job), `receive` (take
the job from stdin), `tail` (follow the log from an offset), `manifest` (the
report, the log's length, and each package's name and size), and `send` (one
package's bytes). `agent/transfer`'s module docs say what each carries. The
Docker worker uses none of them.

---

## The worker's side

`aurcache-worker-container` is what a worker needs to run an agent-built
job, whatever engine its containers come from, and nothing of one engine:

- **job to spec**: a `ClaimedJob`, the repository section, a CPU count and a
  keyserver become a `BuildSpec`. The `pacman.conf` gets the AURCache
  repository appended here, by `repo::append_to_pacman_conf`, as before,
  and the spec carries the job's own beside it, untouched, for the upgrade.
- **the inputs**: the spec and the source archive, as files in a directory.
- **report to completion**: one mapping from how the container ended to a
  `CompleteReport`, through `report::ended`, so a build ends for the same
  reason whatever engine ran it. It takes the worker's stop (cancel,
  timeout) first, then the report, then what the engine said when there is
  no report.
- **the out-of-memory wording**, naming the worker's own limit and its
  variable;
- **what is not supported**: a job that asks for a persistent build
  directory is built in a fresh tree, and only a debug line on the worker
  says so. The setting is on by default, so anything louder would be on
  every build.
- **where the agent is** on the worker's machine, and the constants a
  container's renderer needs: the capability list, the paths inside the
  container.
- **the SSH key**: starting an `ssh-agent` that has it for a build, and
  serving the build's link to that agent ("The SSH link").

It is a crate of its own, and not a module of `aurcache-worker-core`, because
core is by its own account "everything that makes a process a worker" and
"no notion of how a package is built"; the chroot worker would link code it
never calls. It is not part of the agent's crate either: the agent must stay
small and free of the worker's dependencies (an HTTP client, TLS, an async
runtime), and the mapping needs core's types.

The agent's crate exposes its interface types as a library, which is how a
worker and the agent cannot disagree about a field.

### What the chroot worker shares

Two pure pieces move to `aurcache-common`, where the agent can reach them
without depending on a worker crate, and the existing callers keep their
paths: `build_deps`, which dependencies a build installs, read from
`makepkg --printsrcinfo`'s output and the job's flags, so a package cannot
build on one worker and miss a dependency on the other; and `parallelism`,
the `makepkg.conf` lines that size a build's jobs to its CPU limit. The
order a git ref's candidates are tried in moves too, for the agent's VCS
commits: it has to match the server's, or the two ends disagree about a
repository with a branch and a tag of one name. Nothing else of the chroot
worker changes.

---

## The Docker worker on the agent

For each job the worker now:

1. makes `<BUILD_ARTIFACT_DIR_LOCAL>/<worker>_<build_id>/`, mode 0755 (it no
   longer has to be writable by a stranger's uid), with `in/spec.json`,
   `in/source.tar.gz` and a copy of the agent in `bin/`;
2. creates the container:

   | | was | is |
   |---|---|---|
   | user | `ab` | `0` |
   | capabilities | the engine's default set | all dropped, then the seven added |
   | `no-new-privileges` | no | yes |
   | entrypoint, command | the image's, `bash -leco pipefail <script>` | `/work/bin/aurcache-build-agent build --work /work` |
   | binds | the build directory at `/output`; `pacman.conf` and the mirrorlist over `/etc` | the build directory at `/work` |
   | limits, network, platform, name | | unchanged |

3. streams the container's output into the build log, as before;
4. reads `out/report.json`, and asks the engine whether the container was
   killed for memory when there is none;
5. uploads `pkg/` with `upload_artifacts`, as before (the packages are on a
   disk the worker has anyway), and removes the directory.

What an operator sees change:

- **Builds run without sudo and without capabilities.** The paragraph in the
  documentation about qemu's `C` flag goes: nothing in a build is setuid any
  more, and the agent itself is not emulated.
- **Builds report their peak memory, their disk use and their VCS commits**,
  and an out-of-memory failure says so, naming `MEMORY_LIMIT`.
- **A failure before makepkg** (the mirror is down) is reported as the
  worker's, and the server ends the build and queues a new one, where it used
  to fail the package. A PGP key that cannot be fetched is not such a
  failure: the build goes on without it. Nor is a dependency that cannot be
  read or installed: that fails the build, as it does on the chroot worker.
- **The worker stops its builds when it is told to stop**, and reports them
  as interrupted, so the server queues them again at once.
- **The worker names its containers and build directories after itself**,
  so that two instances can share a daemon and a build directory.
- **A custom builder image** needs pacman, makepkg, gpg and `find`, and no longer
  needs sudo or an `ab` user. Every image that worked still works.

Still not supported: a persistent build directory (every build starts from a
fresh tree, silently), kept failed builds, caches. SSH build credentials
are supported ("The SSH link").

### With Podman, rootless engines, and the hybrid image

The hybrid image's own Podman (`userns = "host"`, `cgroupns = "host"`) takes
the same container options through its Docker-compatible socket. Its builds
find their cgroup through `/proc/self/cgroup` (step 9 above). Under a
rootless or user-namespaced engine, root in the container is not the host's
root; the hand-back's `chown` is to an id inside that mapping, and if it is
refused the agent falls back to making `pkg/` and `out/` writable by
anyone, which is what the directory was before.

---

## Threat model, for the Docker worker

The Docker worker's boundary is still the engine's: whoever holds the socket
is root on its host, and the worker holds it. Nothing here changes that.
What changes is inside the build container:

| | was | is |
|---|---|---|
| build code as root in the container | one `sudo pacman -U` away | not directly: no `sudo`, no capabilities, `no_new_privs`, and nothing of root's to ask once makepkg runs; but the PKGBUILD names the packages root installs before the build, any the synced repositories have, and a package's install scriptlet runs as root |
| root in the container, when something does run as root (a dependency's install script) | the engine's default capabilities (14 on Docker) | seven |
| what the build can write that the worker reads | the whole shared directory, world-writable | as the build user, `pkg/` only; as root in the container (a dependency's install script), anything in the directory, before the build starts |
| how the worker reads it back | by path, following links, waiting on pipes, without a limit | one name at a time from its parent's handle, following no link and waiting on no pipe: the report a regular file of at most 1 MiB, believed only beside an agent that exited 0, which writes it after every other process is dead; the packages regular files in a `pkg/` that is a directory of its own, uploaded from the handle they were judged by |
| who can stop the build | the engine, and anything in the container | the engine; root in the container can end it early, as interrupted, but cannot forge its result: PID 1 has no signal handler and cannot be traced from inside, but its resource limits and `oom_score_adj` can be changed |
| a setuid file left in the build directory | possible, as root in the container | still possible (Docker cannot mount a bind `nosuid`); the directory builds go in is made `0700`, and the worker warns when it is open to others, so nobody else on the host can run one |
| what the build can make root do | `pacman` with any arguments, `chmod` with any arguments | before makepkg starts, install packages by name from the job's repositories; once it has, nothing |
| what the build leaves where root's pacman runs | its processes and its files | its files only: what the PKGBUILD writes to `/tmp`, `/var/tmp`, `/dev/shm` or its home while `--printsrcinfo` sources it is still there when root's pacman and the dependencies' scriptlets run. Minor: pacman makes its own temporary directories with `mkdtemp` and runs scriptlets chrooted at `/`, so making use of it takes a scriptlet or hook in a repository package that trusts a fixed path in `/tmp` |
| the worker's SSH key | not offered | never in the container, though whatever runs there while makepkg does can have it sign ("The SSH link") |

A link in `pkg/` named like a package is no package, and a `pkg/` that is a
link is no package directory: `discover_artifacts` opens both without
following them, and the upload sends the file it opened. That is shared with
the chroot worker, whose package directory is its own extraction of the
source.

---

## Testing

- Reading the dependencies from a `.SRCINFO` (shared with the chroot
  worker) and the `makepkg.conf` rendering are pure functions with table
  tests.
- The spec and the report are frozen as JSON fixtures, read and written by a
  test, as the worker protocol's messages are: a worker can be handed an
  agent of another build.
- `scripts/test-build-agent.sh` runs the agent in the published builder
  image under the exact flags above: `docker/testpkg/hello` builds, and a
  hostile fixture checks, while its `.SRCINFO` is printed and from inside
  `build()`, what it must not be able to do, finding nothing of root's to
  talk to either time but the SSH agent socket. Both polarities, as
  `test-sandbox.sh` does it: the fixture's dependencies are only there if
  root installed them, one from a repository of the test's own that stands
  for AURCache's and that the image's upgrade must not see. The same
  fixture, its `.SRCINFO` forged to name a package file and pacman's
  options, fails at its `dependencies` step with nothing of its making run
  as root. Its source comes over the SSH link, from a stand-in for the
  worker, and `build()` reaches the server again and has the agent sign.
  `build()` and a dependency's root scriptlet look for the key and find
  nothing; an agent of the scriptlet's own shows that the hunt works.
- The link, the worker's `ssh-agent` and what the link passes on have unit
  tests, against the real `ssh-agent`.
- The Docker worker's container options are rendered by a pure function,
  tested for never asking for `privileged` and for the exact capability
  list.
- End to end, the hybrid suites build through the agent
  (`test-e2e-hybrid.sh` with the image's own Podman,
  `test-e2e-hybrid-legacy.sh` against the host's daemon).

## Later

- The chroot worker's in-container script as this agent (`build-runner.md`).
- A standalone image and compose file for the Docker worker, if it is wanted
  outside the hybrid image and the native package.
- Dropping `sudo` and the `ab` user from the builder image, once no released
  worker starts it through its entrypoint.
