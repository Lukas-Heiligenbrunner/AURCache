# Container workers: one build agent for Docker and Kubernetes

Status: **In progress** · Last updated: 2026-11-16 · Spikes run on Docker,
kernel 7.2, with the published `aurcache-builder-v2`

Two workers build each job in a container made from the builder image: the
Docker worker, which asks a Docker (or Podman) daemon for it, and the
Kubernetes worker, which asks a cluster for a pod. This is the design of
what they share: the **build agent**, a small program that runs the build
inside that container, and the worker-side code that talks to it. It is also
where the Docker worker stops being a compatibility path.

Related:

- `kubernetes-executor.md`: the Kubernetes worker, and the spike that showed
  how little a build container needs ("What the build container needs").
- `kubernetes-executor-plan.md`: how both are being built.
- `design/proposed/build-runner.md`: the same move for the chroot worker (the
  PKGBUILD only ever runs inside the build's own container, from inputs that
  are data), which has not been made yet.
- `design/proposed/unprivileged-workers.md`: options F (a root helper with a
  fixed set of verbs) and H (a container per build).
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
Linux 6.7, no privileged container of its own. That is the same property the
Kubernetes worker is being built for, on another engine, and keeping one
while retiring the other would not make sense. What made it the lesser
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

And it measured nothing: no peak memory, no disk, no VCS commits.

---

## Naming

| was | is |
|---|---|
| "legacy container builder", "legacy builder", "compatibility builder" | **Docker worker** |
| `docs/docs/workers/legacy-docker.md` | `docs/docs/workers/docker.md`, with a redirect from the old address |
| `describe_self`: `legacy container (<image>) [deprecated]` | `docker (<image>)` |
| the Workers page highlighting `docker` as deprecated | no highlight |

"Docker worker" is what its `kind` (`docker`), its crate, its binary and its
package already say. "Container worker" would have been right until there
were two of them; the chroot worker also ships as a container image, and the
Kubernetes worker also builds in containers.

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
hold root while the build runs without it, serve the build's dependency
installs over a socket, and decide what it will and will not install. And
both workers need exactly the same thing, so it is written once.

**The worker brings its own agent; it is not part of the builder image.**

- The builder image is released on its own schedule (every build of it is
  `latest`), and may be an operator's own (`BUILDER_IMAGE`). An agent baked
  into it would be whatever version that image happened to have. Brought by
  the worker, the agent is always the worker's own build.
- The builder image stays what 0.6.0's worker expects (the `ab` user, its
  sudo rules, an entrypoint that runs its arguments), so an older Docker
  worker keeps working with a newer image. Nothing in the image changes in
  this work.

The Docker worker copies the agent into the build's directory, which is the
one path it shares with the daemon; the Kubernetes worker has an init
container copy it from the dispatcher's own image into a volume.

**Static**, against glibc, because the container it runs in is not the
worker's: another image, possibly another glibc, possibly another
architecture. `kubernetes-executor.md` ("The agent as a static binary") has
the three runs that settled it: it builds with the toolchains the images
already use, drops privileges correctly, and an x86_64 agent runs natively
in an arm64 container, starting an emulated build.

**An interface number.** The agent and the worker are the same build when
things are set up right, and the Kubernetes worker can be set up wrong (an
agent image named by hand). The spec and the report carry `interface: 1`,
and the agent refuses a spec with any other number, as `aurcache-build`
refuses a worker that expects another interface (`build-runner.md`, step 0).

### The directory it is given

```
<work>/                       given by the worker; the agent takes it for root
  in/spec.json                the BuildSpec: the job, as data
  in/source.tar.gz            the server's source archive, as downloaded
  pkg/                        PKGDEST: what the worker collects
  log/build.log               the build's output, when asked for (--log-file)
  out/report.json             the BuildReport, written last
<scratch>/                    named in the spec; never read by the worker
  src/<pkgbase>/              the extracted source: makepkg's start directory
  build/                      BUILDDIR
  srcdest/                    SRCDEST
  home/                       the build user's home and keyring
  run/                        the pacman helper's socket, the ssh-agent's
```

The worker reads back `pkg/`, `out/report.json` and, if it asked for one,
the log. Nothing else. The scratch directory is the Kubernetes worker's
`/work/scratch`, so that everything a build writes is in the volume its disk
limit is on, and the Docker worker's `/var/lib/aurcache-build`, inside the
container, so that nothing owned by the build user is left in a host
directory.

### `build`, step by step

As PID 1 and root, with `CHOWN`, `DAC_OVERRIDE`, `FOWNER`, `FSETID`,
`SETUID`, `SETGID` and `KILL` and nothing else, and `no_new_privs` set by
the runtime:

1. **Take the directory.** `chown root:root <work>`, mode 0755, remembering
   who owned it. The build user can then create nothing beside `out/`, and
   cannot replace it: the owner of a directory can rename what is in it, and
   on a Docker host the worker's uid may be the build user's.
2. **Read the spec**, refusing another interface number. Check the
   `pacman.conf` as `build-runner.md` checks it (no `CacheDir`, `DBPath`,
   `RootDir`, `GPGDir`, `HookDir`, `LogFile`, no `file://` server), then
   write it and the mirrorlist into `/etc`. Root writes them, so nothing is
   bind-mounted over a packaged file any more.
3. **Keyring and upgrade.** `pacman-key --init` (what the image's entrypoint
   did), then `pacman -Syu --noconfirm`.
4. **The build user.** A line in `/etc/passwd` and `/etc/group` for a fixed
   uid and gid (1001), written directly rather than through `useradd`, unless
   the image already has that uid. The scratch directories and `pkg/` are
   made and given to it.
5. **As the build user**, each through `setgroups`, `setresgid`,
   `setresuid` and `PR_SET_NO_NEW_PRIVS`, which leaves a process with no
   capabilities and no way to regain one: unpack the source archive; import
   the job's `validpgpkeys` from the keyserver into its own keyring; start
   `ssh-agent` and load the credential into it, when the spec names one
   (root reads the key file and pipes it to `ssh-add -`).
6. **The helper.** Root listens on a unix socket in `run/`. See below.
7. **makepkg**, as the build user: `makepkg -s <the job's flags>` in
   `src/<pkgbase>`, with a `makepkg.conf` made of the image's own, the job's
   assignments (minus where things go, which is the agent's), then
   `PKGDEST`, `BUILDDIR`, `SRCDEST`, the parallelism for the spec's CPU
   count (the chroot worker's rule: `MAKEFLAGS`, `OMP_NUM_THREADS`,
   `CARGO_BUILD_JOBS`), `PACMAN_AUTH` on the helper, and the ssh-agent's
   socket. Its output, and pacman's, goes to the agent's stdout and to the
   log file.
8. **Afterwards.** Kill whatever the build left running: as PID 1,
   `kill(-1, SIGKILL)` reaches every other process in the container. Then,
   as the build user, ask git what each VCS source was at (as the build
   user because git on a repository the build wrote runs hooks and
   configuration the build chose).
9. **Measure**: `memory.peak` and `memory.events`' `oom_kill` from the
   container's own cgroup, found through `/proc/self/cgroup` so that a
   container sharing the host's cgroup namespace does not read the host's;
   the bytes under the work and scratch directories.
10. **Hand back.** Write `out/report.json`, then give the work directory,
    and `pkg/`, `log/` and `out/` with the regular files in them, back to
    whoever owned it, readable by anyone. Last, after step 8, so nothing the
    build started can write the report or change a package.

The agent exits 0 when it wrote a report, whatever the build did; the build's
own result is in the report. On `SIGTERM` it kills everything and exits
without one: the worker asked for the stop and already knows.

### The pacman helper

`makepkg -s` installs what is missing through `PACMAN_AUTH`, when it is set.
The agent sets it to its own `pacman-request` subcommand, which sends its
arguments to the root process and exits with pacman's status. Root accepts
`pacman [--noconfirm|--noprogressbar|--needed|--color X] -S --asdeps
<targets>` and nothing else, and runs a command line it builds itself from
the targets, with its own environment. The accepted grammar, and why
`makepkg -s` rather than installing from `.SRCINFO`, is in
`kubernetes-executor.md` ("Installing what makepkg asks for").

The build can therefore make root install any package from the job's
repositories as a dependency. It cannot make root install a file, remove a
package, or run pacman with other options. That is the whole of what the
build can ask root for.

### The report

```rust
pub struct BuildReport {
    pub interface: u32,
    pub ended: Ended,
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
    /// A step before makepkg failed, so the package was never built.
    Setup { step: SetupStep, message: String },
}
```

A container that ends with no report was killed whole (out of memory, with
`memory.oom.group`; the node; the operator), and the worker says so from what
the engine tells it.

### What else it does

For the Kubernetes worker, which has no shared directory and reaches the pod
only through `exec`, the same binary has the other end of each exchange:
`stage` (install itself into a volume, wait for the job), `receive` (take
the job from stdin), `tail` (follow the log from an offset), `manifest` (the
report, and each package's name, size and SHA-256), `send` (one package's
bytes), `hold` (keep the pod up), and `probe` (try addresses a build should
not reach). `kubernetes-executor.md` describes them where they are used. The
Docker worker uses none of them.

---

## The worker's side

`aurcache-worker-container` is what both workers need to run an agent-built
job, and nothing either needs alone:

- **job to spec**: a `ClaimedJob`, the repository section, a CPU count and a
  keyserver become a `BuildSpec`. The `pacman.conf` gets the AURCache
  repository appended here, by `repo::append_to_pacman_conf`, as before.
- **the inputs**: the spec and the source archive, as files in a directory
  (Docker) or as one tar (Kubernetes).
- **report to completion**: one mapping from how the container ended to a
  `CompleteReport`, through `report::ended`, so a build ends for the same
  reason whichever worker ran it. It takes the worker's stop (cancel,
  timeout) first, then the report, then what the engine said when there is
  no report.
- **the out-of-memory wording**, naming the worker's own limit and its
  variable.
- **what is not supported**: the line in the build log and the worker
  warning for a job that asks for a persistent build directory.
- **where the agent is** on the worker's machine, and the constants both
  renderers need: the capability list, the paths inside the container.

It is a crate of its own, and not a module of `aurcache-worker-core`, because
core is by its own account "everything that makes a process a worker" and
"no notion of how a package is built"; the chroot worker would link code it
never calls. It is not part of the agent's crate either: the agent must stay
small and free of the worker's dependencies (an HTTP client, TLS, an async
runtime), and the mapping needs core's types.

The agent's crate exposes its interface types as a library, which is how the
workers and the agent cannot disagree about a field.

### What the chroot worker shares

Two pure functions move to `aurcache-common`, where the agent can reach them
without depending on a worker crate, and the existing callers keep their
paths through re-exports: `is_artifact` (which names are packages), and the
order in which a git ref's candidates are tried, which has to match the
server's or the two ends disagree about a repository that has a branch and a
tag of one name.

Nothing else of the chroot worker changes. Its `oom_reason` knows two limits
and three causes that only it has; its parallelism rule is fifteen lines
tied to a comment about `WORKER_BUILD_CPUS`; its credential lines are
written around a socket bound into a chroot. The agent has its own, and
when `build-runner.md` gives the chroot worker an in-container script of the
same shape, that is the moment to give it the agent's.

---

## The Docker worker on the agent

For each job the worker now:

1. makes `<BUILD_ARTIFACT_DIR_LOCAL>/<build_id>/`, mode 0755 (it no longer
   has to be writable by a stranger's uid), with `in/spec.json`,
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
- **A failure before makepkg** (the mirror is down, the keyserver is) is
  reported as the worker's, and the build is queued again, where it used to
  fail the package.
- **`makepkg -i` and `-r` in a package's build flags stop working**: the
  helper installs dependencies and nothing else.
- **A custom builder image** needs pacman, makepkg and gpg, and no longer
  needs sudo or an `ab` user. Every image that worked still works.

Still not supported: a persistent build directory (now with a warning), kept
failed builds, caches. SSH build credentials are something the agent can do
and the Docker worker does not yet offer.

### With Podman, rootless engines, and the hybrid image

The hybrid image's own Podman (`userns = "host"`, `cgroupns = "host"`) takes
the same container options through its Docker-compatible socket. Its builds
find their cgroup through `/proc/self/cgroup` (step 9 above). Under a
rootless or user-namespaced engine, root in the container is not the host's
root; the hand-back's `chown` is to an id inside that mapping, and if it is
refused the agent falls back to making `pkg/`, `log/` and `out/` writable by
anyone, which is what the directory was before.

---

## Threat model, for the Docker worker

The Docker worker's boundary is still the engine's: whoever holds the socket
is root on its host, and the worker holds it. Nothing here changes that.
What changes is inside the build container:

| | was | is |
|---|---|---|
| build code as root in the container | one `sudo pacman -U` away | not reachable: no `sudo`, no capabilities, `no_new_privs` |
| root in the container, when something does run as root (a dependency's install script) | the engine's default capabilities (14 on Docker) | seven |
| what the build can write that the worker reads | the whole shared directory, world-writable | `pkg/` only; the report is root's until the build is dead |
| what the build can make root do | `pacman` with any arguments, `chmod` with any arguments | install named packages from the job's repositories |

A symlink in `pkg/` named like a package is still refused twice, by
`discover_artifacts` and by the `O_NOFOLLOW` open.

---

## Testing

- The helper's argument validation, the `pacman.conf` check and the
  `makepkg.conf` rendering are pure functions with table tests.
- The spec and the report are frozen as JSON fixtures, read and written by a
  test, as the worker protocol's messages are: the Kubernetes worker's agent
  can be a different build from its dispatcher.
- `scripts/test-build-agent.sh` runs the agent in the published builder
  image under the exact flags above: `docker/testpkg/hello` builds, and a
  hostile fixture checks from inside `build()` what it must not be able to
  do. Both polarities, as `test-sandbox.sh` does it: the fixture's
  dependency does install through the helper.
- The Docker worker's container options are rendered by a pure function,
  tested for never asking for `privileged` and for the exact capability
  list.
- End to end, the hybrid suites build through the agent
  (`test-e2e-hybrid.sh` with the image's own Podman,
  `test-e2e-hybrid-legacy.sh` against the host's daemon).

## Later

- SSH build credentials on the Docker worker (a key file bound read-only for
  root; the agent does the rest).
- The chroot worker's in-container script as this agent (`build-runner.md`).
- A standalone image and compose file for the Docker worker, if it is wanted
  outside the hybrid image and the native package.
- Dropping `sudo` and the `ab` user from the builder image, once no released
  worker starts it through its entrypoint.
