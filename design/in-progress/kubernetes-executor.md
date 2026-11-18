# Building in Kubernetes, one pod per build

Status: **Built, not yet merged** (branch `feat/kubernetes-executor`) · Last
updated: 2026-11-16 · Kubernetes facts checked against the v1.36
documentation; container spikes run on Docker, kernel 7.2; the worker run in
kind v0.33 (Kubernetes 1.37, one node, kindnet)

Everything under "Design" is built and covered by `cargo test`, by
`scripts/test-build-agent.sh` and, in a real cluster, by
`scripts/test-e2e-kube.sh`. What is left before this moves to `implemented/`
is under "Still open": user namespaces have not run in any cluster, nothing
has run on more than one node or on arm, and CI has not run the new jobs.

Design for a worker that builds each job in a short-lived pod of its own, with
**no privileged pod and no kernel or filesystem requirement on the nodes**
beyond what an ordinary cluster already has. It is the unprivileged
alternative to running the chroot worker as a privileged StatefulSet, which
stays the full-featured way to run on Kubernetes and is documented
separately.

Two documents go with this one:

- `container-workers.md`: the build agent that runs inside the build
  container, and what the Kubernetes worker shares with the Docker worker.
  Everything about the root step, the privilege drop and the pacman helper is
  there, because both workers run it.
- `kubernetes-executor-plan.md`: how this is being built, work package by
  work package.

Related:

- `design/proposed/unprivileged-workers.md`: the threat model ("What counts as
  contained") and options A to H, of which this is a cluster-shaped option G/H.
- `design/proposed/signed-repository.md`: worker approval is the real trust
  decision.
- `design/proposed/build-runner.md`: the in-container build script the build
  agent shares most of its shape with.
- `design/in-progress/worker-protocol-versioning.md`: why nothing here may
  change protocol 1 except by adding optional fields. Nothing here does.
- `design/implemented/`: `remote-workers.md`, `worker-routing.md`,
  `worker-configuration.md`, `build-disk-quota.md`, `overlay-chroot.md`,
  `stale-shared-pacman-cache.md`, `persistent-build-directory.md`,
  `abort-builds.md`.
- Upstream issues: Lukas-Heiligenbrunner/AURCache#276 (running without
  `--privileged`), Lukas-Heiligenbrunner/AURCache#352 (refreshing the builder
  image locally).

---

## Where things stand

### What the chroot worker's privileges pay for

The chroot worker builds many packages, concurrently, on one long-lived
machine. Most of what it needs root for exists to keep those builds apart from
each other and from the machine:

| what it gives a build | how | what it needs |
|---|---|---|
| a clean root, every time | a btrfs snapshot of the base chroot (`aurcache-chroot/src/pool.rs`) | root, btrfs, a pool device or image |
| a disk bound while it writes | a simple-quota qgroup per snapshot (`qgroup.rs`) | Linux 6.7+ for simple quotas |
| a memory and CPU bound, a peak figure, a kill switch | a cgroup per build (`aurcache-worker/src/cgroup.rs`) | a delegated or writable cgroup tree |
| a container | `makechrootpkg` → `arch-nspawn` → `systemd-nspawn` | `CAP_SYS_ADMIN`, mounts |
| no reach to the worker port | an `iptables` rule by uid (`build_firewall.rs`) | `CAP_NET_ADMIN` |
| a confined host-side fetch | `makepkg --verifysource` under Landlock (`packaging/patch-makechrootpkg.py`) | Landlock |
| caches: sources, packages, keys, kept trees | subvolumes in the pool, private replicas per job (`cache.rs`) | the pool |

The worker reaches all of that through `sudo` with `NOPASSWD: ALL`, inside a
privileged container: root in the worker is root on the host
(`unprivileged-workers.md`, "What root means in the container").

### The Docker worker

`aurcache-worker-docker` (about 1.25k lines) already builds in a fresh
container per job: it pulls `aurcache-builder-v2`, bind-mounts a per-build
directory, and uploads what landed in it. Its container engine is the host's,
through the Docker socket, which is host root by another route; that is the
one thing a pod does not need.

Until this work it ran the build as the image's `ab` user with
`sudo pacman`, and reported no measurements. It now runs the same build agent
as the pods here, with the same capabilities and the same privilege drop, so
what happens inside a build container is one piece of code for both workers.
See `container-workers.md`.

### What the protocol fixes

From `aurcache-common/src/worker.rs` and the server:

- A worker is **one identity** (an mTLS key approved once), advertising one
  arch list, a concurrency, an affinity and a priority at registration.
- Every heartbeat lists the builds the worker holds
  (`Heartbeat::active_build_ids`). A build the worker **stops listing is
  abandoned**: the server ends it and, while the package has attempts left,
  queues a new build of it under a new number
  (`aurcache-utils/src/worker_heartbeat.rs`). The runner sends its first
  heartbeat before its first claim (`aurcache-worker-core/src/runner.rs:212`),
  listing nothing, so whatever a previous process of the same worker held is
  abandoned the moment a new one starts.
- The heartbeat answer is the only way back: `cancel` lists builds to stop.
  The lease defaults to 60 s against a 15 s heartbeat
  (`aurcache-utils/src/worker_policy.rs:89-90`).
- Artifacts are uploaded under the worker's identity, and the server accepts
  them only for a build that worker holds `ACTIVE`
  (`worker_complete::assert_owned_active`, called from
  `aurcache-api/src/worker.rs:580`). Whoever holds the key can upload a
  package for any job that worker has claimed.
- Resource limits are **settings the worker declares** and the server may set
  (`worker-configuration.md`). They are the server's recommendation to help a
  worker protect itself, not something the server relies on: from the
  server's side it makes no difference if a worker gives a build more.

### The hypothesis

With one short-lived pod per build, Kubernetes provides most of the table
above on its own: each build is its own container from an image, with its own
resource limits, its own ephemeral storage, its own network policy, and is
thrown away at the end. So the btrfs pool and the privileges may not be
needed, which would make the worker run on unprivileged clusters and on older
kernels at once.

---

## The hypothesis, part by part

| chroot worker | in a pod | verdict |
|---|---|---|
| snapshot of a refreshed base chroot | a new container from the builder image | **replaced**; freshness becomes the image's problem (see "The build environment") |
| per-build qgroup | `ephemeral-storage` limit and `emptyDir.sizeLimit`, enforced by eviction | **replaced, best-effort**: the build is stopped shortly after it passes the limit rather than refused at it. Acceptable, since the limit is a recommendation |
| per-build cgroup | the container's own cgroup, `resources.limits` | **replaced**, including OOM reporting and a peak figure |
| `systemd-nspawn` | the container runtime | **replaced**; no mounts needed inside the pod at all |
| build firewall | `NetworkPolicy` | **replaced**, where the cluster's CNI enforces it |
| Landlock around the host-side fetch | nothing: the fetch runs inside the build pod, which holds nothing else | **not needed** |
| caches | none, or cluster-level substitutes | **lost** in a first version; see "Caches" |
| kept failed chroots | none | **lost** in a first version |
| `sudo pacman` inside the build | a root step before the build, then a drop to an unprivileged user | **needs root in the pod for one step**, so Pod Security `baseline`, not `restricted` (see the spike) |

So the hypothesis holds, with two qualifications: the build pod cannot meet
the `restricted` Pod Security Standard, and the caches do not come for free.
Neither needs a privileged pod, a btrfs filesystem or a recent kernel.

---

## What the build container needs: a spike

`makepkg` refuses to run as root, and installs dependencies through
`sudo pacman`. The question is how little the container can be given for
both. Run 2026-11-16 with Docker on an Arch host (kernel 7.2.11), using the
published `aurcache-builder-v2` image (pacman 7.1.0, util-linux 2.42.4) and a
trivial PKGBUILD with `makedepends=('tree')`. Each Docker flag stands for the
pod field it becomes. The scripts were throwaway; the flags are what matter.
Every row is `docker run --rm --entrypoint /bin/bash <flags>
aurcache-builder-v2 <script>`, where the script for A to C is
`sudo pacman-key --init; sudo pacman -Sy; makepkg -s`, and for D, E and G is,
as root:

```sh
useradd -u 1001 -m -d /var/kxbuild kxbuild
pacman-key --init
pacman -Sy --noconfirm --needed --asdeps tree polkit
chown -R kxbuild: /var/tmp/b && cd /var/tmp/b
setpriv --reuid=kxbuild --regid=kxbuild --init-groups --no-new-privs --inh-caps=-all \
    env HOME=/var/kxbuild makepkg --noconfirm
cat /sys/fs/cgroup/memory.peak
```

| | container | result |
|---|---|---|
| A | `--user ab` (default capabilities) | `sudo pacman` and `makepkg -s` work: how the Docker worker built before it moved to the agent |
| B | `--user ab --cap-drop=ALL` | `sudo: unable to change to root gid: Operation not permitted`; `makepkg -s` fails to install its dependency |
| C | `--user ab --security-opt no-new-privileges` (`allowPrivilegeEscalation: false`) | `sudo: The "no new privileges" flag is set, which prevents sudo from running as root`; build fails |
| D | `--user 0`, default capabilities; root installs the dependency, then `setpriv --reuid --regid --init-groups --no-new-privs --inh-caps=-all` to a build user, `makepkg` without `-s` | **works**; the build runs with `CapEff 0`, `NoNewPrivs 1`, and `sudo` refuses |
| E | `--user 0 --cap-drop=ALL --security-opt no-new-privileges` | pacman installs, but cannot set ownership (`Can't set user=0/group=102 for /etc/polkit-1/rules.d`, failing ACL and `fchownat` in hooks); `setresuid` to a build user fails; `makepkg` as root refuses ("Running makepkg as root is not allowed") |
| G | `--user 0 --security-opt no-new-privileges --cap-drop=ALL --cap-add=CHOWN,DAC_OVERRIDE,FOWNER,FSETID,SETUID,SETGID,KILL`, then the drop of D | **works**: `polkit` installs with `root:polkitd` ownership, the build user has `CapEff 0` with `NoNewPrivs 1`, the package is produced, and `/sys/fs/cgroup/memory.peak` is readable from inside (121 MB) |

Two things follow.

**`restricted` is out.** The `restricted` standard requires `runAsNonRoot`,
`allowPrivilegeEscalation: false`, dropping `ALL` capabilities and adding back
at most `NET_BIND_SERVICE` (Pod Security Standards page, "Restricted"). Row C
shows sudo cannot work under the second rule, rows B and E that nothing can
install dependencies with no capabilities, and E that an unprivileged build
user cannot be reached from a capability-less root. User namespaces do not
change that: a pod with `hostUsers: false` is exempt from the `runAsNonRoot`
and `runAsUser` checks, but "with the Restricted pod security standard, a pod
still must only use the default or empty ProcMount", and the capability rule
still applies (User Namespaces page, "Integration with Pod security admission
checks"). Row E is exactly what `restricted` plus user namespaces allows.

**`baseline` is enough, and less than it allows.** Row G's seven capabilities
are all on `baseline`'s allowed list (`AUDIT_WRITE`, `CHOWN`,
`DAC_OVERRIDE`, `FOWNER`, `FSETID`, `KILL`, `MKNOD`, `NET_BIND_SERVICE`,
`SETFCAP`, `SETGID`, `SETPCAP`, `SETUID`, `SYS_CHROOT`), and the container
runs with `allowPrivilegeEscalation: false`. So the pod meets every
`restricted` control except running as root and the capability list. The
build code itself runs with no capabilities, no setuid path to root, and no
`sudo`. That is **stronger than the chroot worker**, and than the Docker
worker as it was, where the build user can `sudo pacman -U` a package it
made and so become root wherever it runs (`unprivileged-workers.md`, "The
paths to host root", step 2).

Not settled by the spike: `CAP_SYS_CHROOT` was not needed here (the
`sysusers` hook ran and created `polkitd` without it), but whether libalpm
skips `chroot(2)` for a root of `/` was not checked in its source, so a
package whose scriptlet fails without it may yet turn up. Whether any common
dependency needs `MKNOD` or `SETFCAP` (file capabilities, e.g. `iputils`) is
untested too. `setpriv --bounding-set=-all` needs `SETPCAP`, so it was left
out rather than granting that (the move to a non-root uid already clears the
effective and permitted sets, and `no_new_privs` keeps them from coming back). All three are on `baseline`'s list and can be added if
a package needs one.

### The agent as a static binary

The spike used shell. What ships is a binary, `aurcache-build-agent`, that has
to run in whatever image the build uses, so two more things were checked the
same day, with a throwaway Rust program that prints its own capabilities and
spawns a child through `setgroups`/`setresgid`/`setresuid` and
`PR_SET_NO_NEW_PRIVS`:

| | run | result |
|---|---|---|
| S1 | built for `x86_64-unknown-linux-gnu` with `-C target-feature=+crt-static`, bind-mounted read-only into `aurcache-builder-v2`, row G's flags | `static-pie linked`, 1.7 MB; root has `CapEff 0xfb` (the seven), the child is uid 1001 with `CapEff 0`, `NoNewPrivs 1` |
| S2 | the same x86_64 binary in a `--platform linux/arm64` container (`busybox`), on an x86_64 host with `qemu-aarch64` registered | the agent runs natively; the child it spawns reports `aarch64`, with `CapEff 0` and `NoNewPrivs 1` |
| S3 | built for `armv7-unknown-linux-gnueabihf` the same way, with the AUR cross toolchain | links (`statically linked`); not run |

So one statically linked agent serves any builder image, glibc version and
all, and on a host that builds foreign architectures through binfmt the
agent stays native while everything it starts is emulated. Static linking
against glibc needs only `libc.a`, which Arch's `glibc`, its
`aarch64-linux-gnu-glibc`, the AUR's `arm-linux-gnueabihf-glibc` and Debian's
`libc6-dev-*-cross` all ship, so it uses the toolchains the images already
build with. The agent resolves no user names and no host names, which is what
static glibc cannot do.

### Installing what makepkg asks for

`makepkg -s` installs dependencies through `run_pacman -S --asdeps <deps>`,
which prefixes `PACMAN_AUTH` when it is set (`/usr/bin/makepkg`, lines
228-246 and 291 in pacman 7.1; `PACMAN_AUTH=()` is documented in
`/etc/makepkg.conf`). The command it builds is
`<PACMAN_AUTH...> /usr/bin/pacman <PACMAN_OPTS...> -S --asdeps <deps...>`,
where `PACMAN_OPTS` is whatever of `--noconfirm`, `--noprogressbar` and
`--color never` makepkg was given.

So the build keeps using `makepkg -s`, with
`PACMAN_AUTH=(/aurcache/bin/aurcache-build-agent pacman-request)`: a client
that sends its argv over a unix socket to the root process. That process
accepts one shape only, and runs a command line of its own rather than the
one it was sent:

- the first word is pacman's path, then any of `--noconfirm`,
  `--noprogressbar`, `--needed`, `--color <never|auto|always>`;
- then `-S --asdeps`, exactly;
- then one or more targets, each a package name (`[A-Za-z0-9@_+][A-Za-z0-9@._+-]*`)
  with an optional version constraint (`<`, `<=`, `=`, `>=`, `>` and a
  version of `[A-Za-z0-9._+:~-]+`), which is what `pacman -T` hands back to
  makepkg.

Everything else is refused: `-U`, `-R` (so `makepkg -r` and `-i` do not work
here, in a container that is thrown away anyway), paths, `repo/name`,
`--config`, `--root`, `--dbpath`, `--overwrite`, `--hookdir`, anything
starting with a dash after `--asdeps`. That keeps makepkg's own dependency
rules (arch-specific and split-package dependencies, version constraints)
instead of reimplementing them, and it is option F's "root helper with a
fixed set of verbs" (`unprivileged-workers.md`) at its smallest. The
alternative, installing everything `.SRCINFO` names before the build, is
simpler and is the fallback if the helper's validation proves fragile.

What the root step still installs is chosen by the build: any package in the
job's sync repositories, including the AURCache repository. A hostile package
already published there runs its install scriptlet as root in the pod of
every build that depends on it. That is true of the chroot worker today, and
it is why user namespaces matter below. It is also why the SSH build
credential is never in the pod, and why its socket is gone before any
dependency is installed ("Credentials and PGP keys").

---

## Where the code lives

Three places were considered for the executor: a Kubernetes backend inside
the Docker worker, the chroot worker's job logic with a pod instead of
nspawn, and a crate of its own.

**Inside the Docker worker** looks natural, since that worker is "a container
per build from an image" already. Little of it carries over: `executor.rs` is
bollard calls around bind mounts of host paths, which pods do not have;
`network.rs` is Docker network plumbing; `config.rs` resolves the
host-visible and container-visible paths of a shared directory. And
`executor.rs` in `aurcache-worker-core` is explicit that "each executor lives
in its own crate with its own binary, so neither has to know the other
exists".

**The chroot worker's job logic** (`aurcache-worker/src/job.rs`, 1.26k lines)
is the richest build flow, but it is the pool, the caches and devtools all
the way through: a `Lease` from the pool, `JobCaches` replicas, `SRCDEST`
locks, cgroup placement, the `aurcache-build` script, staging credentials for
a host-side fetch. A pod changes each of those, so what would be reused is
shape, not code.

**A crate of its own** is what is built: `aurcache-worker-kube`, a binary on
`aurcache-worker-core` like the other two. It reuses from core, unchanged:
enrollment, the `Runner` (claim within concurrency, heartbeat, self-abort,
one terminal report), `BuildLog`, `WorkerClient::upload_artifact`,
`report::ended`, `Timeout`/`StopWatch`, and `repo::append_to_pacman_conf`.

What the Docker worker and this one do have in common is everything on the
build's side of the container boundary, and the little on the worker's side
that speaks to it. That is shared, in two more crates
(`container-workers.md` has the reasoning):

| crate | what it is | who uses it |
|---|---|---|
| `aurcache-build-agent` | the statically linked binary that runs in the build container (root step, privilege drop, pacman helper, makepkg, report), and the types of its interface | the binary: both workers' build containers. The types: both workers |
| `aurcache-worker-container` | the worker's side of an agent-run build: job to `BuildSpec`, report to `CompleteReport`, the out-of-memory wording, where the agent binary is | `aurcache-worker-docker`, `aurcache-worker-kube` |
| `aurcache-worker-kube` | the dispatcher: settings, pod rendering, the cluster seam, the pod's lifecycle, streaming hand-back | itself |

The chroot worker is not touched, beyond two pure functions moving to
`aurcache-common` so the agent can use them without a second copy
(`is_artifact`, and the order in which a git ref's candidates are tried).

Rough size, tests included:

| part | lines |
|---|---|
| dispatcher: config and declared settings | 250 |
| pod rendering (pure: job and config in, `Pod` out) | 350 |
| cluster seam: create, watch through a reflector, exec, delete, sweep, dry run | 450 |
| lifecycle, hand-in, log tail, streaming hand-back, status-to-report mapping | 700 |
| build agent: root step, pacman helper, drop, ssh-agent, keyring, report | 900 |
| build agent: stage, receive, tail, manifest, send | 300 |
| shared worker side | 350 |
| tests (rendering, mapping, helper argv validation, a fake cluster, the agent in Docker) | 1200 |
| **total** | **about 4.5k** |

---

## Design

### Shape: one dispatcher, many build pods

```
            mTLS (one approved identity)
 server  <-------------------------------  dispatcher (Deployment, 1 replica,
                                           unprivileged, `restricted`)
                                              |  Kubernetes API, namespace-scoped
                                              v
                                     build pods (namespace aurcache-builds,
                                     PSA `baseline`, NetworkPolicy, no SA token)
```

The **dispatcher** is the worker, from the server's point of view: one
approved identity that enrolls, registers with `KIND = "kubernetes"`, claims
jobs, heartbeats and reports. Its `run_job` creates a pod for the job, feeds
it, follows it, relays its packages and turns how it ended into the report.
It never builds, needs no capabilities, and passes `restricted` itself. The
pods never talk to the server's worker port, never see the dispatcher's key,
and hold no credential for any job, their own included.

**Builds run in a namespace of their own** (`WORKER_KUBE_NAMESPACE`, default
`aurcache-builds`), labelled for Pod Security `baseline`. The dispatcher's
namespace is not affected by what builds need and can be `restricted`. Since
an owner reference cannot cross namespaces, nothing collects a build pod when
its dispatcher goes away; "Restarts and orphans" covers that.

**One replica, `strategy: Recreate`.** Two processes with the same identity
would each list only their own builds in their heartbeats, and each heartbeat
abandons whatever the other holds. A rolling update does exactly that for a
moment. More capacity means a higher `WORKER_CONCURRENCY` or a second
dispatcher with an identity of its own, never `replicas: 2`.

**Its identity** lives where the other workers keep theirs, in its data
directory, so on a small `ReadWriteOnce` PVC. It is the only thing the
dispatcher stores: packages never touch its disk (see "Getting things in and
out").

### Concurrency and `ready_for_work`

`WORKER_CONCURRENCY` is the most build pods at once, one total for every
architecture, as on every worker; the runner's gate already enforces it. The
question `ready_for_work` answers is different: whether the cluster can start
one more *now*, because a claimed job holds a lease the server expects
progress on (`executor.rs`, `ready_for_work`). The dispatcher keeps a
`kube::runtime::reflector` store of its own pods (a single watch, no API call
per check) and answers no while:

- any of its pods is `Pending` with `PodScheduled=False`, reason
  `Unschedulable`: the cluster is full, and another claim would only queue a
  second job behind it;
- the cluster would not admit a build pod. Before each claim the dispatcher
  sends the pod it would create as a server-side dry run (`dryRun=All`),
  which runs admission without creating anything. That is where a
  `ResourceQuota` with no room, a namespace at the wrong Pod Security level,
  a missing ServiceAccount or a policy webhook says no, with a message the
  dispatcher logs once per change. A job claimed into a cluster that refuses
  its pod would be reported `Interrupted` and spend one of the build's
  attempts for nothing. Quota is checked but not consumed in a dry run, which
  the end-to-end suite confirms against a real quota that admits no pod.

A pod that stays unschedulable past `WORKER_KUBE_SCHEDULE_TIMEOUT` (declared,
default 10 minutes) is deleted and reported `Interrupted` ("no node could
take the build"), so the server ends the build and queues a new one, which
another worker may take. A pod a node took gets the same allowance again to
reach its `stage` container, and is reported `Interrupted` the same way
("the build pod did not start"): a volume that cannot be mounted or a pod
network that cannot be set up shows in events, not in the pod's status, and
would otherwise hold the build until its own timeout and be blamed on the
package.

What the API server answers the dry run with is taken as its answer for a
real pod, whatever the code: a 404 for a namespace that is not there, a 500
from a webhook that is down and a 429 hold claims back like a 403 does. Only
a question that reached no server leaves the dispatcher claiming. The
build timeout counts from the claim, as it does on every worker, so time
spent pending is time the build does not get; that is the honest accounting,
since the server's job timeout is about how long a build holds its lease.

### Arches

The advertised arch list is configuration (`WORKER_ARCHES`), as on every
worker, not discovery; unset, it is the dispatcher's own architecture. Each
job's pod gets
`nodeSelector: {kubernetes.io/os: linux, kubernetes.io/arch: <arch>}`, mapped
as the Docker worker maps platforms (`x86_64` → `amd64`, `aarch64` →
`arm64`, `armv7h` → `arm`, `riscv64` → `riscv64`; the `arm` label carries no
variant). The dispatcher never narrows the list on its own, since a node pool
scaled to zero is still capacity; a job for an architecture no node has ends
at the schedule timeout. Emulated builds (binfmt on the nodes) stay the
operator's business: the protocol carries one arch list and nothing about how
a worker builds it (`RegisterRequest::arches`).

The agent image has to exist for every architecture builds run on, since the
`stage` and `hold` containers of a pod run on that pod's node.

Extra scheduling (`tolerations`, `affinity`, more `nodeSelector` keys, a
`priorityClassName`, a `runtimeClassName` such as gVisor or Kata,
`imagePullSecrets`) comes from a file the operator mounts
(`WORKER_KUBE_POD_TEMPLATE`), read as those fields and no others: it cannot
add a volume, a container or a security context.

### Restarts and orphans

When the dispatcher restarts, its first heartbeat lists nothing, and the
server abandons every build it held. The pods those builds ran in are then
orphans, and the dispatcher deletes them before it enrolls: one
`deletecollection` by label selector `aurcache.io/dispatcher=<fingerprint>`.
That loses the progress of builds that were running, which is what every
worker does on a restart today. Running pods are **not adopted**: resuming
one would need a way to hand the runner a job it did not claim, which it
does not have, and is left for later. The server ends each abandoned build
and queues a new one for its package, under a new number, while the package
has attempts left.

The pods also bound themselves without the dispatcher:
`activeDeadlineSeconds` is the build's timeout plus an hour, so a pod
outlives a dead dispatcher by at most that. The hour is the hand-back's: the
timeout stops the build and not the upload of what it made, and a deadline
ten minutes past it cut large uploads off.

While it runs, the dispatcher also deletes, before each claim, any pod of its
own that no build of its holds: one whose delete failed when its build ended,
and a pod of no build that has ended. It keeps the set of
builds it is seeing through for that. Such pods would otherwise stand until
their deadline, and count against a quota.

**Told to stop** (`SIGTERM` from the kubelet, `SIGINT`), the dispatcher does
the same work itself instead of leaving it to its next start. It is the first
process of its container, where a signal nobody listens for is dropped, so it
listens from its first moment. The runner then claims nothing more and trips
every build's token (each is a child of one the runner holds, so a build
claimed at that moment stops too); each build deletes its pod as for a
cancel, and the runner reports it `Interrupted` rather than cancelled, which
makes the server queue a new build at once instead of when a lease runs out.
The dispatcher allows forty seconds for that, inside the Deployment's sixty,
then deletes by label whatever pod of its own is left (`Runner::stop` in
`aurcache-worker-core`; `main.rs`).

### Cancellation and timeouts

The runner trips the job's `CancellationToken` for a heartbeat `cancel` and
for the timeout, as for every executor. The dispatcher then deletes the pod
with a short grace period (`gracePeriodSeconds: 5`) and reports `canceled` or
the timeout through `report::ended`, as the Docker executor does with
`kill_container`.

### The build pod

```yaml
apiVersion: v1
kind: Pod
metadata:
  name: aurcache-build-<dispatcher>-<build_id>
  namespace: aurcache-builds
  labels:
    app.kubernetes.io/managed-by: aurcache-worker-kube
    aurcache.io/dispatcher: <first 16 hex digits of the dispatcher's fingerprint>
    aurcache.io/build-id: "<build_id>"
    aurcache.io/arch: <arch>
spec:
  restartPolicy: Never
  activeDeadlineSeconds: <timeout + 1 h>     # absent for a build with no timeout
  serviceAccountName: aurcache-build         # bound to nothing
  automountServiceAccountToken: false
  enableServiceLinks: false
  hostUsers: false                           # only with WORKER_KUBE_USER_NAMESPACES=true
  nodeSelector: {kubernetes.io/os: linux, kubernetes.io/arch: <arch>}
  securityContext:
    seccompProfile: {type: RuntimeDefault}
  initContainers:
  - name: stage                              # installs the agent, receives the job, exits
    image: <agent image>
    command: [/usr/local/bin/aurcache-build-agent, stage, --work, /work, --tools, /aurcache/bin]
    securityContext: &unprivileged
      runAsUser: 65534
      runAsGroup: 65534
      runAsNonRoot: true
      allowPrivilegeEscalation: false
      readOnlyRootFilesystem: true
      capabilities: {drop: [ALL]}
    volumeMounts: [{name: work, mountPath: /work}, {name: tools, mountPath: /aurcache/bin}]
  containers:
  - name: build
    image: <builder image>
    imagePullPolicy: Always
    command: [/aurcache/bin/aurcache-build-agent, build, --work, /work, --log-file, /work/log/build.log]
    securityContext:
      runAsUser: 0
      allowPrivilegeEscalation: false
      capabilities:
        drop: [ALL]
        add: [CHOWN, DAC_OVERRIDE, FOWNER, FSETID, SETUID, SETGID, KILL]
    resources:                               # each line only when its limit is set
      requests: {cpu: <build_cpus>, memory: <build_memory_max>, ephemeral-storage: <build_disk_max>}
      limits:   {cpu: <build_cpus>, memory: <build_memory_max>, ephemeral-storage: <build_disk_max>}
    volumeMounts:
    - {name: work, mountPath: /work}
    - {name: tools, mountPath: /aurcache/bin, readOnly: true}
  - name: hold                               # keeps the pod up for the hand-back
    image: <agent image>
    command: [/usr/local/bin/aurcache-build-agent, hold]
    securityContext: *unprivileged
    resources: {requests: {cpu: 10m, memory: 16Mi}, limits: {memory: 64Mi}}
    volumeMounts: [{name: work, mountPath: /work, readOnly: true}]
  volumes:
  - name: work
    emptyDir: {sizeLimit: <build_disk_max>}  # no sizeLimit without a disk limit
  - name: tools
    emptyDir: {sizeLimit: 64Mi}
```

Notes on the choices:

- **The agent comes from the dispatcher's image**, copied into `tools` by the
  `stage` init container. The builder image is then the stock
  `aurcache-builder-v2`, untouched, and the agent is always the dispatcher's
  own: the manifests name it in `WORKER_KUBE_AGENT_IMAGE`, kept equal to the
  dispatcher's image by the kustomization. The dispatcher can read it from
  its own pod's status instead, by digest (`WORKER_KUBE_POD_NAME` and
  `WORKER_KUBE_POD_NAMESPACE`, from the downward API), but that takes `get`
  on the pods of its namespace, the server's among them, so the manifests do
  not grant it. The
  agent refuses a job written for another interface number, so a mismatch is
  an error message and not a misread field.
- **The builder image's `sudo` grants are inert** under
  `allowPrivilegeEscalation: false`, and its entrypoint is not used: the
  agent is the container's command and does what the entrypoint did.
  It is pulled `Always` (only the manifest is checked when the layers are
  present); `WORKER_KUBE_IMAGE_PULL_POLICY` says otherwise for an image
  loaded onto the nodes by hand.
- **The build user is the agent's**, added at start with a fixed uid, not the
  image's `ab`, whose uid is whatever `useradd --system` picked when the
  image was built (968 in the published one).
- **No Secret** of any kind is mounted: the SSH build credential stays with
  the dispatcher ("Credentials and PGP keys").
- **`hold`** is a regular container rather than a native sidecar, because a
  native sidecar is stopped as soon as the build container exits, and the
  dispatcher needs the pod up after the build to take the packages out. It
  never runs build code, and mounts the work volume read-only. The dispatcher
  deletes the pod when it is done.
- **The pod's name carries the dispatcher**, so two dispatchers sharing a
  namespace cannot collide on a build id, which is only unique per server.

### Inside the build container

`aurcache-build-agent build`, as PID 1 and root, is described step by step
in `container-workers.md`: it writes the pacman configuration, upgrades the
image, adds the build user, drops to it for the keys, the sources and
`makepkg -s`, serves makepkg's dependency installs through the pacman
helper, kills whatever the build left running, and writes its report. Here it
is given `/work` (the `work` volume) for everything, the build trees
included, so all of a build's disk use is in the one volume the size limit
is on; and `--log-file`, so its output is in a file the dispatcher can follow
from any offset.

Its report is a file, `/work/out/report.json`, which the build user cannot
write. It is not the container's termination message: the kubelet keeps at
most 4096 bytes of that, and a report carries a commit per VCS source.

### Getting things in and out

The pod needs the job in (source archive, configuration, keys) and packages
out, and must never hold anything that lets it act as the worker. Both go
through the Kubernetes API, with `pods/exec`: no listener, no Service, no
token, and the pod reaches nothing in the cluster, so its NetworkPolicy can
say so. The cost is `create` on `pods/exec` in the builds' namespace, and
that packages travel through the API server.

**In.** The dispatcher downloads the job's source archive from the server
(kilobytes to a few megabytes: a PKGBUILD's directory), and execs
`aurcache-build-agent receive --work /work` in the `stage` container, writing
a tar of two entries to its stdin: `spec.json` (the `BuildSpec`) and
`source.tar.gz`. `receive` reads up to the tar's end-of-archive marker, so it
does not depend on the exec's stdin being half-closed, writes both under
`/work/in`, and marks the directory ready; `stage` sees the mark and exits,
and the build container starts.

**Out, streamed.** The dispatcher has no disk for packages and must not need
one: a package can be tens of gigabytes, and the dispatcher's only volume
holds its key. So a package is never a file on the dispatcher. It goes from
the pod to the server as one pipe:

```
 hold container                dispatcher                         server
 aurcache-build-agent  stdout  exec stream   -->  request body    POST /api/worker/jobs/<id>/artifacts/<name>
   send --name <name>  ------> (WebSocket)        (chunked read)  -> staged on the server's disk
```

1. After the build container exits, the dispatcher execs
   `aurcache-build-agent manifest --work /work` in `hold`. It answers with
   the report and a manifest, as JSON: for every regular file in `/work/pkg`
   whose name passes `is_artifact`, opened with `O_NOFOLLOW`, its name, its
   size and its SHA-256. Nothing is running in the build container by then,
   so the files cannot change afterwards.
2. For each entry, the dispatcher execs
   `aurcache-build-agent send --work /work --name <name>`, whose stdout is
   the file's bytes and nothing else, and hands that stream to
   `WorkerClient::upload_artifact` as the request body, with the manifest's
   size as `Content-Length`. Bytes are read from the exec as the upload
   takes them, so the pod is slowed to the server's pace and the dispatcher
   holds a few kilobytes at a time: kube's bounded stdout pipe and one
   request chunk.
3. As the bytes pass, the dispatcher counts and hashes them. An upload only
   counts when the server accepted it, the exec ended cleanly, and the count
   and digest match the manifest.

What the server's route needs was checked against
`aurcache-api/src/worker.rs` (`job_artifact`), and it already fits:

| | the route |
|---|---|
| files per request | one; the name is the last path segment |
| body | the raw file, streamed to the server's staging directory as it arrives; no multipart |
| size up front | optional: with `Content-Length`, a file over the package's `max_artifact_size` is refused before any of it is sent (`413`). The manifest supplies it |
| digest | none asked for, before or after. The SHA-256 here is the dispatcher's own check on a path with three hops in it (kubelet, API server, dispatcher), not something the server sees |
| who may | the worker holding the build `ACTIVE`: the dispatcher's own identity |

So streaming needs **no protocol change**: no new field, no new route.

When it goes wrong: a file the server refuses for what it is (`400`, `413`)
fails the build, as on every worker (`PackagesRefused`). Anything else (a
broken exec stream, a short read, a digest that does not match, a server
that restarted and asked the worker to register again, which a streamed body
cannot be replayed through) is retried once from a fresh `send`, which
overwrites the staged file; a second failure reports the build
`Interrupted`. A streamed request has no response deadline
(`WorkerClient::upload_http`), and the lease is kept by the runner's
heartbeat meanwhile, as for any upload.

What is not known: throughput through an API server and a kubelet for a
20 GiB package. The stream is not buffered anywhere, so it is a question of
speed and of how proxies in front of the API server treat a long upgrade,
not of memory. The end-to-end test sends a multi-gigabyte artifact through
kind to put a number on it. "Future: uploading straight from the pod" is
what would replace this path where it proves too slow.

Two alternatives were set aside. One tar of every package through a single
exec would save an exec per file, but the dispatcher would have to cut the
stream into request bodies, and a failed file could not be sent again alone.
A push to the dispatcher (an in-cluster endpoint and a per-job token) needs
no `pods/exec`, but adds a listener, a Service, a NetworkPolicy hole from the
builds to the dispatcher, and a relay that is no shorter.

### Logs

The agent writes the build's output to `/work/log/build.log` (and to its
stdout), and the dispatcher follows the file with an exec of
`aurcache-build-agent tail --work /work --from <offset>` in `hold`, pumping
it into `BuildLog` as the Docker executor pumps `attach`. When the stream
breaks (the kubelet's `streamingConnectionIdleTimeout`, 4 h by default, ends
silent sessions) it starts another from the byte it last forwarded. `tail`
ends by itself once the report exists and it has reached the end of the log.

The kubelet's log API (`pods/log`, `follow=true`) is not what the build log
is read from: following stops at log rotation on some kubelets
(k3s-io/k3s#9060; OpenShift's OCPBUGS-38861, fixed in OpenShift's kubelet),
only the newest file is served, so a build past the kubelet's
`containerLogMaxSize` (10 MiB by default) can lose lines, and it gives no way
to resume from a known point. It is read once, for its last lines, when a
build container ends without a report, so that what the agent or the runtime
said on the way down reaches the build log.

Before the agent runs there is no log: the dispatcher writes what the pod's
status says (scheduled on which node, pulling which image, why it is waiting)
into the build log itself, as `[worker]` lines.

### How a build ended

From the report, the build container's `state.terminated` and the pod's
`status`, through the reflector:

| observed | report |
|---|---|
| a report: makepkg exited | `report::ended(None, elapsed, Exit::Code(..))`, with `peak_memory_bytes`, `disk_usage.workdir` and `vcs_commits` from the report; a non-zero exit with `oom_kill` events in the container's cgroup is worded as out of memory |
| a report: a step before makepkg failed (the sync, the keys, the user) | `Interrupted`, naming the step: the worker's problem, not the package's |
| no report, `reason: OOMKilled` | failed, out of memory, naming `build_memory_max` (on cgroup v2 the kubelet sets `memory.oom.group` on each container unless `singleProcessOOMKill` is set, so an OOM kills the whole container, the agent included; Kubernetes blog, "The Shift to cgroup v2", 2026-10-06, and kubernetes/kubernetes#117793) |
| pod `reason: Evicted`, for passing its own limit (the message says a limit was exceeded) | failed, "out of disk: the build passed its ephemeral-storage limit of N and was evicted" |
| pod `reason: Evicted`, for anything else (the node was short) | `Interrupted` |
| pod `reason: DeadlineExceeded` | timed out (the backstop; the dispatcher's own timeout normally fires first) |
| unschedulable past the timeout, not started within it once scheduled, `ErrImagePull`/`ImagePullBackOff` for two minutes, `CreateContainerConfigError`, creation refused | `Interrupted`, with the status message |
| the pod disappears (node lost, deleted by hand), or its build container ends with neither a report nor `OOMKilled` | `Interrupted` |

Only a build that left a report carries measurements. None carries the
chroot's share of the disk: the container's writable layer is not visible
from inside, so `DiskUsage::chroot` is `None`.

### Credentials and PGP keys

**SSH build credentials** never enter a build pod. The dispatcher holds the
key and answers for it; a build can ask for signatures, for as long as it is
fetching its sources, and never obtains the key.

The key and the hosts' keys come from a Secret the operator provides, in the
dispatcher's own namespace, mounted into the dispatcher's pod alone (the key
under `ssh-privatekey`, as in a `kubernetes.io/ssh-auth` Secret, and
`known_hosts` beside it). `WORKER_GIT_SSH_KEY` and `WORKER_SSH_KNOWN_HOSTS`
name the two files, as on the chroot worker. `known_hosts` is required: a pod
is new for every build, so an ssh that accepts the first key a host shows
would accept any host, every time. The dispatcher refuses to start with a key
and no host keys, and with a key it cannot use (protected by a passphrase, or
of a type it does not sign with).

Why not in the pod: a key mounted `0400` into the build container, root's
alone, and loaded into an `ssh-agent` running as the build user, is kept from
the build *user* and from nothing else that runs as root in the pod. A
dependency's install scriptlet runs as root there (see "What the root step
still installs" above), and one that copied the mounted key out was shown to
work. One deploy key
shared by every build of a dispatcher, taken by any package in any repository
a build installs from, is not a boundary.

**The dispatcher's agent.** The dispatcher reads the key at the start of each
build (so a rotated Secret takes effect at the next build) and answers the
agent protocol itself, in process. It answers two requests and refuses every
other one with `SSH_AGENT_FAILURE`:

- `SSH_AGENTC_REQUEST_IDENTITIES`: the one public key;
- `SSH_AGENTC_SIGN_REQUEST` for that key, and only over data that parses as an
  SSH `publickey` login (RFC 4252's request, or OpenSSH's
  `publickey-hostbound-v00`): Ed25519; RSA as `rsa-sha2-256` or `rsa-sha2-512`,
  by the request's flags (SHA-1 `ssh-rsa` signatures are refused, as OpenSSH
  and GitHub refuse them); ECDSA on P-256 and P-384. A link is given 64
  signatures at most, and 16 in any minute.

Adding, removing or locking keys, extensions (`session-bind@openssh.com`
among them, which OpenSSH's client treats as optional) and everything else are
refused, so a build cannot change what another is served. The keys are read
from the unencrypted OpenSSH format `ssh-keygen` writes. Signing is *ring*'s
(constant-time, RSA blinded), already in the dependency tree through rustls;
the RustCrypto `rsa` crate is not used, as its signing is not constant-time
(RUSTSEC-2023-0071) and a build can ask for as many signatures as it likes.
OpenSSH's own `ssh-agent` would have been the other way, behind a proxy that
lets the same two requests through; it needs `ssh-agent` and `ssh-add` in the
dispatcher's image, a socket and a child process, for no gain in what a build
can do.

**The link.** There is no filesystem shared by the dispatcher and a pod, and
the NetworkPolicy keeps builds from reaching anything in the cluster, so the
requests travel over an exec stream, as everything else between the two
does. The agent in the pod (PID 1, root) is the one end of it:

1. With `ssh` in the spec, the agent first listens on `link.sock` in a
   directory only root can enter (`<scratch>/ssh-link/`), before anything of
   the job runs.
2. Once the build container runs, the dispatcher execs
   `aurcache-build-agent ssh-link --socket <scratch>/ssh-link/link.sock` into
   it, **once per build, and never again**, whatever becomes of that exec.
   `ssh-link` connects (retrying for a minute, for an agent that has not
   listened yet) and copies bytes between the socket and its stdin and
   stdout; it understands nothing.
3. When the build needs the key (below), the agent accepts that one
   connection, checks the peer is root, closes the listener, and the two ends
   exchange a hello (`aurcache-ssh-link 1`). The agent then listens on
   `<scratch>/ssh/agent.sock`, owned by the build user and mode `0600`, in a
   directory of root's that only the build group can enter: the same reach
   as the chroot worker's socket.
4. Each agent request a build-user connection sends (a length-prefixed
   message of at most 256 KiB, OpenSSH's own limit) goes to the dispatcher as
   a frame with an id, and the answer comes back under the same id; the
   dispatcher keeps no state per connection. Requests are answered one at a
   time per build.
5. To close it, the agent stops listening, removes the socket, shuts every
   connection, sends a closing frame, and kills every other process in the
   container (`ssh-link` among them) before anything else runs. The
   dispatcher stops at the closing frame or the end of the stream, and since
   it execs once, nothing can open the link again.

If the exec never comes, or the link breaks before the agent closes it, the
build stops at its `credential` step and is reported as interrupted, so the
server builds it again; it is not reported as the package's failure.

**When the socket is up.** Only while the sources are fetched, and before
anything is installed for the build:

1. As root, before the build user has any process: what the root step always
   does, then `pacman -S --needed openssh git` from the job's repositories.
2. As the build user: unpack the source, import the PGP keys.
3. **The window.** The link is accepted and the socket opened; makepkg runs
   once as `makepkg --verifysource --skipinteg` (plus `--ignorearch` when the
   job's flags have it), which downloads every source into `SRCDEST`, VCS
   clones included, and does nothing else: no dependency is installed, no
   function of the PKGBUILD runs. The pacman helper refuses every request
   while this runs.
4. Every process but the agent is killed, `ssh-link` included, and the link
   is closed as above.
5. The helper opens, and makepkg runs as it always does, `makepkg -s
   --holdver <the job's flags>`, without `SSH_AUTH_SOCK`: `--holdver` keeps it
   from updating the VCS clones it now finds in `SRCDEST`, and the files are
   there already, so it downloads nothing it fetched in step 3.

That download is best-effort: makepkg can fail there where the build itself
would not, for a VCS source whose client (`mercurial`, `subversion`,
`breezy`, `fossil`) is only installed with the build's `makedepends`. The
build goes on, and makepkg fetches what is missing in step 5 the way it would
have, without the key. Only `git` and `openssh` are installed for step 3, as
git is the transport of nearly every source that needs a key.

**What a build can still do with the key, and who.** Code that can connect to
the socket while it is up can log in with the key, to any host, up to a budget
(64 signatures, 16 a minute); the agent signs an SSH login request and nothing
else, so the key cannot sign a commit, an SSHSIG or a blob of the build's
choosing. It can never obtain the key, which is in no file, no process and no
memory of the pod. During the window that code is the
package's own: the PKGBUILD's top level as makepkg sources it, makepkg, git
and ssh, all as the build user, and only for a build that has a source
fetched over SSH (`makepkg --printsrcinfo` says, and the window does not open
otherwise). Nothing root ran before the window can sign in it: once the link
is taken, every process but the agent and the one `ssh-link` carrying it is
killed, and `/proc` is checked to hold no other, before the socket opens. A
daemon the image's upgrade or the install of `openssh` and `git` left running
is gone by then, and the socket is root's at a mode root would ignore, so
there is nothing for a later one to connect to either. Dependencies, their
install scriptlets, their hooks, and `prepare()`, `build()`, `check()` and
`package()` all run after the link is gone, and can neither sign nor read
anything. A dependency's root scriptlet that leaves a process behind finds no
socket, and no exec to take over: `ssh-link` was killed before it ran, and the
dispatcher opens no second one.

Compared with the chroot worker: its socket is bound into the chroot for the
whole build, and `makechrootpkg` downloads the sources on the host with it as
well, so there the dependencies' scriptlets (root in the chroot), `prepare()`
and `build()` can all ask for signatures too, and the key is protected from
them only by being on the host. The container workers give less time and
fewer parties, at one cost: a PKGBUILD that clones over SSH in `prepare()` or
`build()` rather than through `source=` (the chroot worker's comment names
`unreal-engine`) cannot authenticate here. That is deliberate. Supporting it
means leaving the socket up while dependencies install, and so while code
from any repository the build installs from runs as root in the pod, which is
the exposure this removes.

It is one key per dispatcher, given to every build it runs, as one key per
chroot worker is; `WORKER_PACKAGES` routes the packages that need it.

The generated-key onboarding (`docs/docs/workers/credentials.md`) does not
carry over: a key generated in a pod dies with it. A dispatcher generating
one into a Secret it owns would need write access to Secrets, and is left
out.

**PGP keys.** The agent imports the job's `validpgpkeys` from the keyserver
per build, into the build user's own keyring (`WORKER_KEYSERVER`, declared,
as on the chroot worker). No shared writable keyring in any pod, which is
what the chroot worker's per-job replicas work hard to emulate. A key that
cannot be fetched is a line in the build log and no more, as on the chroot
worker: `validpgpkeys` often names keys no source of the version is signed
with, and a build that needs the missing one fails at makepkg's check. Later, the
dispatcher can fetch each key once over HKP, keep it, and ship the armored
keys with the job, so builds stop depending on the keyserver being up.

### Limits

Limits are applied as well as Kubernetes allows, and never refused for being
imperfect: they are the server's recommendation to help the worker protect
the cluster, not a guarantee anything relies on.

**Disk** (`build_disk_max`, declared): the build container's
`ephemeral-storage` limit (its writable layer, where pacman installs
dependencies, and its logs) and `emptyDir.sizeLimit` on `/work`. Both are
enforced by eviction: "If a Pod is using more ephemeral storage than you allow
it to, the kubelet sets an eviction signal that triggers Pod eviction"
(Ephemeral Storage page). So a build is stopped some seconds after it passes
the limit, not refused at it, and a build that writes faster than the kubelet
measures can, for that moment, take the node's disk past what was planned
(`fallocate -l 1T` fails on the node's free space, not on the limit). The
kubelet's own `nodefs` eviction thresholds then protect the node. The request
of the same size makes the scheduler place builds where there is room. That
is a weaker bound than the chroot worker's qgroup, and an acceptable one.

Optional hardening, for operators who want writes refused: a generic
ephemeral volume (`ephemeral.volumeClaimTemplate`, GA in 1.23) of
`build_disk_max` from a StorageClass whose provisioner enforces size (block
volumes: cloud disks, TopoLVM, OpenEBS LVM), mounted at `/work`, plus a
low `ephemeral-storage` limit for the rest. A writer then gets `ENOSPC`, as
under a qgroup. It costs a provision and attach per build (seconds to tens of
seconds on cloud disks), and directory-backed provisioners such as
local-path-provisioner do not enforce size at all (unverified against its
current release). A `medium: Memory` emptyDir also refuses writes past its
size, but "files you write count against the memory limit of the container
that wrote them" (Volumes page), so it suits small builds only.

The total (`disk_max` on the chroot worker) has no executor equivalent; a
namespace `ResourceQuota` on `requests.ephemeral-storage` is the operator's
tool, and `ready_for_work` honours it.

**Memory and CPU** (`build_memory_max`, `build_cpus`): `resources.limits`,
with requests equal to them so a build gets what it is told it has. CPU
sizes `MAKEFLAGS` by the chroot worker's rule, which the build agent
applies. No swap setting is declared. With requests equal to limits, a limit
is also a reservation: a build told it has 16 CPUs waits for a node with 16
free. All three are unset by default.

**Persistent build directories** (`persistent_builddir`) are not supported:
a pod's volume goes with the pod. A job that asks for one is built in a
fresh tree rather than refused, since the package builds either way and only
its rebuild time differs. Nothing is said about it beyond a debug line on the
worker: the setting is on for every package that does not turn it off, so a
warning would be on every build. **Kept failed builds** are not supported either: this
worker declares no `keep_failed` setting, so there is nothing to turn on,
and the documentation says so.

### Network

Every build pod gets a NetworkPolicy, applied to the namespace:

```yaml
apiVersion: networking.k8s.io/v1
kind: NetworkPolicy
metadata: {name: aurcache-builds, namespace: aurcache-builds}
spec:
  podSelector: {matchLabels: {app.kubernetes.io/managed-by: aurcache-worker-kube}}
  policyTypes: [Ingress, Egress]
  ingress: []                               # nothing connects to a build
  egress:
  - to: [{namespaceSelector: {}, podSelector: {matchLabels: {k8s-app: kube-dns}}}]
    ports: [{protocol: UDP, port: 53}, {protocol: TCP, port: 53}]
  - to: [{namespaceSelector: {matchLabels: {kubernetes.io/metadata.name: aurcache}},
          podSelector: {matchLabels: {app.kubernetes.io/name: aurcache,
                                      app.kubernetes.io/component: server}}}]
    ports: [{protocol: TCP, port: 80}, {protocol: TCP, port: 8081}]
                                            # the repository, never the worker port
  - to:
    - ipBlock:
        cidr: 0.0.0.0/0
        except: [10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16, 100.64.0.0/10, 169.254.0.0/16]
    - ipBlock:
        cidr: ::/0
        except: [fc00::/7, fe80::/10]
```

The ports are the server pod's own, which is what a NetworkPolicy matches,
not its Service's: 80 for the nginx container the Kubernetes manifests put in
front of the repository, 8081 for the server's built-in handler. The shipped
policy shuts builds out of every private range rather than naming one
cluster's pod, service and node ranges; an operator whose mirror or keyserver
is on a private address adds a rule for it.

- The worker port is what `build_firewall.rs` closes on the chroot worker: a
  build that reaches it can submit a rogue registration. Allowing the
  repository's port and nothing else of the server does the same. When the
  server is outside the cluster and shares one address and port between the
  repository and the worker protocol (TLS passthrough on 443), neither the
  firewall nor a NetworkPolicy can separate them, which is the existing
  limitation; worker approval stays the trust decision
  (`signed-repository.md`).
- `169.254.0.0/16` keeps builds off the cloud metadata endpoint, which on a
  managed cluster without workload identity hands out the node's
  credentials. This matters more than anything else on the list.
- The API server, the dispatcher and every other workload are cluster
  addresses, so they fall in the `except` ranges. With exec-based hand-back,
  builds need nothing from the dispatcher.
- A NetworkPolicy only does something when the CNI enforces it (Calico,
  Cilium, kube-router, kindnet; not flannel alone). The dispatcher cannot
  tell from the API whether it is enforced; the documentation says how to
  check by hand.
- A CNI may apply a policy to a pod only some seconds after the pod starts.
  kindnet lets a new pod reach everything for about ten seconds. A build
  pod's own first seconds are spent waiting for the job and then in the
  agent's root step, before any package code runs.

### The build environment

The chroot worker upgrades its own base chroot and keeps the previous one; a
pod starts from whatever its image is. The options:

| | cost | freshness |
|---|---|---|
| the published `aurcache-builder-v2`, `pacman -Syu` at the start of every build | the upgrades since the image was built, downloaded by every build, since nothing is cached | always current; the Docker worker has always done this |
| an image the dispatcher rebuilds in the cluster | a builder: kaniko is archived upstream (June 2025, forks continue), buildah needs user namespaces or privileges of its own | current to the last rebuild; a privileged-or-userns builder is what this design avoids |
| an image the operator's CI rebuilds and pushes on a schedule | outside AURCache | as fresh as the schedule, which is Lukas-Heiligenbrunner/AURCache#352's concern turned over to the operator |
| an init container that upgrades into a volume | the whole root on a volume, copied per build | no better than `-Syu`, much larger |

**Decided: the published image plus `-Syu` per build**, with
`imagePullPolicy: Always` on its tag (only the manifest is checked when the
layers are present). Correct by construction, and the cost is bandwidth, which
a caching mirror in the cluster (a pacman caching proxy Deployment, given to
builds through the mirrorlist AURCache already distributes) removes for
everyone at once. The proxy caches the official repositories only, never the
AURCache repository, so it cannot reintroduce the stale-shared-cache problem
(`stale-shared-pacman-cache.md`): AURCache's own packages can change bytes
under one filename, and the official ones do not.

### Caches

| cache | chroot worker | first version | later |
|---|---|---|---|
| pacman packages | shared read-only plus a private writable one per job | none; every build downloads its dependencies | the caching mirror above |
| sources (`SRCDEST`) | a subvolume per pkgbase | none; every build fetches its sources | a PVC per pkgbase with access mode `ReadWriteOncePod` (GA in 1.29, KEP-2485), which makes the deferred shared-`SRCDEST` race impossible by construction: a second pod for the same pkgbase waits until the first lets go |
| PGP keyring | shared plus per-job replicas | none; keyserver per build | keys shipped by the dispatcher |
| kept build trees | subvolumes | none | the same `ReadWriteOncePod` PVC |

**What dropping them costs**, not measured here: a full dependency download
per build (base-devel is in the image, so this is the package's own
`depends`/`makedepends`, from a few MB to several GB for toolchains), and a
full source fetch. For tarball sources that is usually small; for VCS
packages it is a full clone every time, which for large repositories (LLVM,
Chromium-sized trees) is minutes and gigabytes per build, and it is the cache
whose loss hurts most. Per-node `hostPath` caches are ruled out: `hostPath`
breaks the isolation that is the point here ("If you allow a read-write mount
of any host path by an untrusted Pod, the containers in that Pod may be able
to subvert the read-write host mount", Volumes page), `baseline` forbids it,
and a writable cache shared between concurrent builds brings back every race
the chroot worker's cache code exists to avoid. A `ReadWriteMany` cache has the
same races across nodes.

### The PKGBUILD on the worker's side

The chroot worker runs `makepkg --verifysource` on the host, under Landlock,
because that is the one moment PKGBUILD code runs beside other builds' files
(`packaging/patch-makechrootpkg.py`). In the pod there is no host side: the
fetch runs inside the build pod, as the build user, like `build()` (the same
move `build-runner.md` makes for the chroot worker). The pod holds nothing
of anyone else's, so Landlock is not needed. The dispatcher never parses a
PKGBUILD; the server's parse keeps `aurcache-sandbox`.

Landlock remains possible inside the pod as defence in depth, since it needs
no privilege, but nothing here depends on it.

### User namespaces

With `hostUsers: false`, root in the pod is an unprivileged uid range on the
node: the root step's capabilities, a hostile dependency's scriptlet and any
container escape all land as nobody in particular. That closes the main gap
the pod model shares with the chroot worker (root in the build's container is
root on the host's uid 0).

Requirements (User Namespaces page, Kubernetes v1.36, where the feature is
"Stable since Kubernetes v1.36" and first appeared in v1.28):

- Linux 6.3 or later on the nodes, "as tmpfs started supporting idmap mounts
  in that version", and idmap support on the filesystems under
  `/var/lib/kubelet/pods/` and in every volume (btrfs, ext4, xfs, fat, tmpfs,
  overlayfs; not NFS);
- containerd 2.0 or later, or CRI-O 1.25 or later; runc 1.2 or later, or crun
  1.9 or later (1.13 recommended);
- no `hostNetwork`, `hostIPC`, `hostPID` or `volumeDevices`, none of which
  this pod uses.

**Off by default, and the operator's to turn on**:
`WORKER_KUBE_USER_NAMESPACES=true` sets `hostUsers: false` on every build
pod. Off, the worker runs on any cluster; the documentation recommends it
on, and the dispatcher logs at warning severity, once at startup, that
builds run without it. It is read from the dispatcher's environment only and
is not a declared setting: it is part of what a build may reach, and a
server that could set it could turn it off. Whether the root step works
unchanged under it is the one part of the spike not yet run in a user
namespace; rootless Podman builds with `sudo` in `unprivileged-workers.md`
(option H) are the strongest evidence that it does, and the end-to-end test
runs once with it on where the cluster under test supports it.

---

## Kernel requirements, end to end

| feature | kernel floor | used |
|---|---|---|
| containers, capabilities, `no_new_privs`, `setresuid` drop | any kernel a supported Kubernetes runs on | yes |
| cgroup v2 | whatever the cluster requires (cgroup v1 is being removed from the kubelet, KEP-5573) | yes, through the runtime |
| `memory.peak` | 5.19 (from the kernel's cgroup v2 changelog; not rechecked here) | optional: the peak figure is `None` below it |
| user namespaces in pods | 6.3, plus idmap-capable filesystems | optional hardening |
| Landlock | 5.13 (filesystem), 6.7 (TCP) | no |
| btrfs simple quotas | 6.7 | no |

So **the design works on 5.x and 6.1 LTS nodes**, including managed node
images, with two features degrading rather than failing: the peak memory
figure (below 5.19) and user namespaces (below 6.3). Nothing requires
btrfs, a block device, or a filesystem of any particular kind. That is a
statement about what the design needs, not a test on such a kernel; the spike
ran on 7.2, and the kind job below should pin an older node kernel where CI
allows.

---

## Threat model

Measured against "What counts as contained" in `unprivileged-workers.md`:
with arbitrary code running as root *in the worker*, the host keeps its
storage, its kernel and its other services.

The worker is now two things.

**The dispatcher** holds the identity and has namespace-scoped RBAC: create,
watch and delete pods, exec into them, read their logs, all in the builds'
namespace. It is unprivileged and passes `restricted`. Compromised, it can do
what any compromised worker can (upload hostile packages for jobs it claims),
and it can create pods, but only pods the builds' namespace admits, which
`baseline` keeps from being privileged, mounting host paths, or joining host
namespaces. **A compromised dispatcher is not node root.** A compromised
chroot worker is host root.

**A build pod**, with arbitrary code as root in it (a hostile dependency's
scriptlet, a bug in the agent's helper):

- *storage*: no host path, no device, no PVC beyond its own; passes;
- *kernel*: no `CAP_SYS_ADMIN`, no module loading, `/sys` and `/proc/sys`
  read-only as in any unprivileged container, the runtime's default seccomp
  profile; passes;
- *other services*: no host network namespace, no runtime socket, no service
  account token, no service environment variables; the cluster and the
  metadata endpoint only as far as the NetworkPolicy is enforced; passes
  where it is.

What remains is the shared node kernel: an escape needs a kernel or runtime
bug, and without user namespaces it lands as the node's uid 0. With
`hostUsers: false` it lands as an unmapped uid.

Against **other builds**, this is stronger than the chroot worker: separate
root filesystems, separate network namespaces, no shared caches, keyring or
pacman cache to poison, and no shared host-side step. Against **the server**:
a pod never holds the worker's key, has no credential for any job, its own
included, and cannot reach the worker port to register.

Compared with the chroot worker, it is **weaker** in:

- the disk limit, which evicts rather than refuses (a trade-off, not a
  containment failure, as "Limits" explains);
- depending on the CNI for the network rule, which the worker cannot verify
  except by probing, where the chroot worker installs and checks its own
  `iptables` rule;
- running without user namespaces by default.

It is **stronger** in: no privileged container anywhere; build code with no
path to root inside its own container (no `sudo`); no PKGBUILD code outside
the build's own container; a dispatcher whose compromise is bounded by Pod
Security admission.

Optional, for operators who want more: a `runtimeClassName` for gVisor (GKE
Sandbox) or Kata, which moves the boundary from the shared kernel to a
sandbox or a VM. Whether pacman and makepkg run under gVisor is untested.

---

## Testing

`kubernetes-executor-plan.md` says which work package adds which test and how
each is run. The layers:

- **Unit tests** for the pure parts, which carry most of the risk: pod
  rendering (job and settings to `Pod`, including that nothing ever renders
  `privileged`, a `hostPath` or an `fsGroup`), the status-to-report mapping,
  and the pacman helper's argument validation (the security-critical piece:
  a table of what makepkg really sends, and of flags and names that must be
  refused).
- **The dispatcher against a fake cluster.** Everything the executor asks of
  Kubernetes goes through one trait (`Cluster`: create, the watched pod,
  exec, delete, sweep, dry run), with the real client behind it. The tests
  drive the lifecycle against an in-memory implementation whose pods change
  state on cue and whose `exec` is a pair of pipes: a build that succeeds
  and streams two packages, a cancel, a pod that never schedules, an
  `OOMKilled` container, an eviction, a log stream that breaks and resumes,
  a digest that does not match. kube's client can also be built over a
  `tower` service with canned HTTP responses, but an exec is a WebSocket
  upgrade, which that cannot stand in for.
- **The agent in a plain container**, with the pod's security context
  translated to flags exactly as in the spike's row G
  (`--user 0 --security-opt no-new-privileges --cap-drop=ALL --cap-add=...`),
  building `docker/testpkg/hello` and a hostile fixture that asserts, from
  inside `build()`, that it has no capabilities, cannot `sudo`, cannot read
  the credential, cannot write the report, and cannot get anything but
  `-S --asdeps <names>` out of the pacman helper. Both polarities: the same
  fixture's dependency installs through the helper. This needs only Docker,
  so it runs on every push beside `test-sandbox.sh`.
- **The Docker worker end to end**, which runs the same agent: the two
  existing hybrid suites (`test-e2e-hybrid.sh`, `test-e2e-hybrid-legacy.sh`).
- **kind** for the Kubernetes worker end to end: a server, the dispatcher and
  the build pods in one cluster, from the shipped manifests, building the
  test package and installing it from the repository. kind's nodes are
  privileged containers, but that only matters for what kind cannot show
  (the node boundary itself); the build pods inside are ordinary unprivileged
  pods under the same admission and the same kubelet, which is what is being
  tested. Its default CNI, kindnet, implements NetworkPolicy (kindnet's
  documentation; its repository was archived in October 2025, so pinning, or
  Calico, is the safer default), so a "build cannot reach the worker port"
  assertion runs too. User namespaces in kind depend on the
  runner's kernel (6.3+; GitHub's Ubuntu 24.04 runners ship 6.8) and the node
  image's containerd (2.0+), both unverified for a specific kind release.
  k3s is the alternative where kind's privileged nodes are unwelcome.
- What CI cannot cover: managed offerings, eviction timing on a real disk,
  and artifact throughput through a production API server.

---

## What it costs operators

- **A namespace** for builds, labelled
  `pod-security.kubernetes.io/enforce: baseline`. The dispatcher's own
  namespace needs nothing of it and can be `restricted`.
- **RBAC**, namespace-scoped. In the builds' namespace, for the dispatcher's
  ServiceAccount:

  ```yaml
  rules:
  - apiGroups: [""]
    resources: [pods]
    verbs: [create, get, list, watch, delete, deletecollection]
  - apiGroups: [""]
    resources: [pods/exec]
    verbs: [create, get]        # get: the WebSocket upgrade on some API servers (KEP-4006); unverified per version
  - apiGroups: [""]
    resources: [pods/log]
    verbs: [get]
  ```

  and nothing in its own namespace: reading its own pod's image there would
  take `get` on every pod in it.
  Builds get a ServiceAccount bound to nothing (`aurcache-build`).
- **A CNI that enforces NetworkPolicy**, or accepting that builds can reach the
  cluster.
- **Storage**: a small `ReadWriteOnce` PVC for the dispatcher's identity, and
  nothing else: packages are streamed through, never stored. A StorageClass
  only for the optional disk hardening and the later caches.
- **Nodes**: nothing. Standard `kubernetes.io/arch` labels; optionally a taint
  for dedicated build nodes.
- **Pulls**: the builder image (about 1 GB compressed) once per node, and the
  dispatcher's image for the agent.

### Managed offerings

- **GKE Autopilot**: "allows running as root to enable most workloads"; the
  default capability set includes all seven the build container adds; it
  blocks privileged containers and denies writable `hostPath`; `RuntimeDefault`
  seccomp is applied to all workloads (GKE Autopilot security measures page).
  So the pod as designed should be admitted. User namespaces are not
  mentioned there (unverified). GKE Sandbox is offered as a stronger
  boundary for untrusted workloads.
- **EKS Fargate**: privileged containers are not supported, capabilities can
  only be dropped from the default set (EKS pod security best practices), and
  each pod gets 20 GiB of ephemeral storage by default, up to 175 GiB (EKS
  Fargate pod configuration page). Each Fargate pod runs in a compute
  environment of its own, a better boundary than a shared node (from the
  EKS Fargate user guide; not re-read for this doc). Whether
  NetworkPolicy is enforced for Fargate pods, and the start-up latency per pod,
  are unverified; arm64 support is unverified.
- **Plain managed node pools** (GKE Standard, EKS, AKS): yes, with the CNI's
  NetworkPolicy support turned on where it is optional.

---

## Wording to revisit elsewhere

Treating limits as best-effort means some existing text describes them as
stronger than the server needs them to be. Not changed here:

- `docs/docs/workers/split-chroot.md` (around line 303): "A worker that has
  limits configured but cannot enforce them **refuses every build**". That is
  the chroot worker's choice for limits pinned on the machine, and it is
  worded as if it were the rule for limits in general. A Kubernetes worker
  would apply what it can and say how.
- `docs/docs/workers/storage-pool.md` ("How it works"): "The kernel enforces
  both limits while the build writes" is right for the pool, but the variable
  table presents `WORKER_BUILD_DISK_MAX` as "Disk one build may write", which
  reads as a guarantee of the setting rather than of this worker.
- The settings declared to the server carry descriptions; the Kubernetes
  executor's `build_disk_max` should say "evicted past this size, best effort"
  so the Workers page does not show it as the same promise.

---

## Future: uploading straight from the pod

Relaying every package through the API server and the dispatcher is the
simplest path that keeps credentials out of the pod. It is also three hops
where one would do. If the relay proves too slow for large packages, the pod
can upload to the server itself. This is not being built now; this section
is what it would take.

**The credential.** A per-job upload grant: a random token the server mints
when the job is claimed, good for one thing, `POST
/api/worker/jobs/<id>/artifacts/<name>` for that one build, for as long as
the build is `ACTIVE` under the worker that claimed it. It ends with the
lease. Leaking it lets someone upload packages for that build, which the
build's own code can already do by making them, so it gives a hostile build
nothing new; it reaches no other job, and cannot claim, report or register.

**The protocol.** Within protocol 1's rules, since everything is optional
and a worker that ignores it loses nothing:

- `ClaimRequest` gains an optional `upload_grant: bool`, so the server mints
  tokens only for workers that will use them;
- `JobDescriptor` gains an optional `upload: { token, ca_pem }` (the CA so
  the pod can verify the server without trusting on first use);
- the artifact route accepts `Authorization: Bearer <token>` as an
  alternative to the worker's certificate. The worker port's TLS already
  makes the client certificate optional (`mandatory(false)` in
  `aurcache-api/src/init.rs`, for enrollment), so this is a second request
  guard, not a second listener. New fixtures are added for the new fields;
  none is rewritten.

The server holds grants in memory, like the protocol agreement. After a
server restart a grant is gone; the upload is refused and the dispatcher
falls back to the relay, which always remains.

**Who holds it in the pod.** Not the build. The token goes to the `hold`
container, which never runs build code, over the exec's stdin, only after
the build container has exited: `aurcache-build-agent upload` reads the
manifest's files and posts them. Completion is still the dispatcher's to
report, once that exec says every file was accepted.

**The network.** This is the real cost. A NetworkPolicy is per pod, not per
container, so letting `hold` reach the server's worker port lets the build
container reach it too, for the whole build. That is the port
`build_firewall.rs` closes on the chroot worker, because whoever reaches it
can submit a registration (which then waits for approval, or is approved
outright where an enrollment token is configured and known). Two ways out:

- a separate upload listener on the server, on its own port, that serves the
  artifact route for bearer tokens and nothing else. The builds' policy
  allows that port and still not the worker port. This is server work: a new
  port, declared in `aurcache_common::server_env` with its documentation
  row, and a route mounted twice;
- or accept the exposure where registrations need a click to approve, and
  document that an enrollment token must not be used with it.

The first is the one to build. It also has to work when the server is
outside the cluster, where the pods' egress to it is whatever the operator's
network allows.

**What it buys.** One TLS connection from the node to the server instead of
kubelet to API server to dispatcher to server, and no load on the API server
that grows with package size.

---

## How it was built

`kubernetes-executor-plan.md` is the plan: the work packages, their
interfaces, their tests and their order. All of them are merged into
`feat/kubernetes-executor`. In one line each:

1. The build agent, with the plain-container test in CI.
2. The shared worker side, and the Docker worker moved onto the agent.
3. `aurcache-worker-kube`, the dispatcher: settings, rendering, the cluster
   seam, the lifecycle, the sweep, readiness. `kube`
   5.1 with the `client`, `runtime`, `rustls-tls` and `ws` features (exec
   needs `ws`), and `k8s-openapi` 0.29 with the oldest API feature it offers,
   `v1_33`, so the generated types cover clusters from 1.33 on. Its rustls
   provider has to match the workspace's.
4. Images, manifests and documentation.
5. kind, as an end-to-end suite of its own.

Left for later, in rough order of worth: `ReadWriteOncePod` source caches;
keys shipped by the dispatcher; the caching mirror's guidance; uploading
straight from the pod; persistent build trees on the same volumes as the
source cache; adoption of running pods across a restart; the dispatcher's
key from a Secret, so it needs no volume; a startup check that a node exists
for each advertised architecture (it needs `list` on nodes, a ClusterRole).

---

## What was decided

| question | decision |
|---|---|
| Pod Security level for builds | `baseline`. `restricted` cannot run a build that installs dependencies (the spike). The build container starts as root with all capabilities dropped but seven, and `allowPrivilegeEscalation: false`; the build itself runs with none |
| how `makepkg -s` installs | through the root-side helper, which accepts `-S --asdeps <names>` and nothing else |
| user namespaces | off by default; `WORKER_KUBE_USER_NAMESPACES=true` turns them on |
| artifact transport | `pods/exec`, streamed from the pod into the upload, nothing staged on the dispatcher. No protocol change. A direct upload from the pod is a later optimisation |
| where builds run | a namespace of their own; cleanup is the startup sweep and `activeDeadlineSeconds` |
| the SSH build credential | a Secret the operator provides, `known_hosts` included, mounted into the dispatcher alone; builds reach it over an exec stream while their sources are fetched, and never hold it |
| caches | none in the first release |
| `persistent_builddir`, kept failed builds | not supported: the first builds in a fresh tree and says nothing, the second is not offered |
| the Docker worker | no longer deprecated, and moved to the same agent, capabilities and privilege drop (`container-workers.md`) |
| adopting pods across a restart | no; leftover pods are swept by label at startup |
| concurrency | one total per worker |
| who is the worker | the dispatcher, alone. Build pods never talk to the server's worker port and hold no credential |
| limits | the server's recommendation, applied as well as Kubernetes allows; a disk limit that evicts rather than refuses is the stated trade-off |

## Still open

- `CAP_SYS_CHROOT`, `MKNOD`, `SETFCAP`: not granted, not yet needed. A
  package whose install scriptlet wants one will say so by failing; all three
  are on `baseline`'s list.
- Whether quota admission is evaluated in a server-side dry run on every
  supported version, which the readiness check assumes. It is on 1.37, where
  the end-to-end suite holds claims back with a quota that admits no pod.
- **User namespaces under the root step have not run anywhere.** The kind
  node on the development machine cannot start a pod with `hostUsers: false`
  (runc fails mounting sysfs), so the suite skips that leg and says why. The
  pod is rendered with the field and nothing else changes, but a build in a
  user namespace is unverified.
- **One node, one architecture.** Nothing has run on a cluster with several
  nodes, where the agent image is pulled per node, or on arm.
- **The agent image is named, not read.** The manifests set
  `WORKER_KUBE_AGENT_IMAGE` to the dispatcher's own image and grant no role
  in the dispatcher's namespace: reading its own pod took `get` on every pod
  there, the server's included. Reading it is still possible
  (`WORKER_KUBE_POD_NAME`), by digest, and that path has unit tests only.
  With a tag that moves, the agent a node pulls may be another build than the
  dispatcher; the interface number is what keeps the two honest.
- **SSH from `prepare()` or `build()`** cannot authenticate: the link is
  closed before dependencies install ("Credentials and PGP keys"). Whether
  some packages need it enough for an opt-in that keeps the link up, with
  dependencies' scriptlets able to sign, is undecided.
- CI has run none of this: the `buildAgentTest` job, the kube-worker image's
  publishing step and the `e2e-kube.yml` workflow are new.

Measured in kind, on one machine: a 2 GiB package went from the end of its
build to published at about 50 MiB/s through `exec`; the dispatcher's peak
memory over that relay was 12 to 14 MiB; a cancelled build's pod was gone 2
to 17 seconds after the cancel.
