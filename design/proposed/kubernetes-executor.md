# Building in Kubernetes, one pod per build

Status: **Proposed** (exploration) · Last updated: 2026-11-16 · Kubernetes
facts checked against the v1.36 documentation; container spike run on Docker,
kernel 7.2

Plan for a worker that builds each job in a short-lived pod of its own, with
**no privileged pod and no kernel or filesystem requirement on the nodes**
beyond what an ordinary cluster already has. It is the unprivileged
alternative to running the chroot worker as a privileged StatefulSet, which
stays the full-featured way to run on Kubernetes and is documented
separately.

Related:

- `design/proposed/unprivileged-workers.md`: the threat model ("What counts as
  contained") and options A to H, of which this is a cluster-shaped option G/H.
- `design/proposed/signed-repository.md`: worker approval is the real trust
  decision.
- `design/proposed/build-runner.md`: the in-container build script this shares
  most of its shape with.
- `design/in-progress/worker-protocol-versioning.md`: why nothing here may
  change protocol 1 except by adding optional fields.
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

### The docker worker

`aurcache-worker-docker` (deprecated, about 1.25k lines) already builds in a
fresh container per job: it pulls `aurcache-builder-v2`, bind-mounts a
per-build directory, runs `sudo pacman -Syu`, imports the `validpgpkeys`, runs
`makepkg -s` as the image's `ab` user (`commands.rs:54-75`), streams the
container's output into the build log, and uploads what landed in the
directory. It has no caches, no credentials, and no measurements
(`peak_memory_bytes`, `disk_usage`, `vcs_commits` are always empty). Its
container engine is the host's, through the Docker socket, which is host root
by another route.

### What the protocol fixes

From `aurcache-common/src/worker.rs` and the server:

- A worker is **one identity** (an mTLS key approved once), advertising one
  arch list, a concurrency, an affinity and a priority at registration.
- Every heartbeat lists the builds the worker holds
  (`Heartbeat::active_build_ids`). A build the worker **stops listing is
  abandoned** and requeued with a fresh attempt
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
| A | `--user ab` (default capabilities) | `sudo pacman` and `makepkg -s` work: the docker worker's model |
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
`sudo`. That is **stronger than both existing workers**, where the build user
can `sudo pacman -U` a package it made and so become root wherever it runs
(`unprivileged-workers.md`, "The paths to host root", step 2).

Not settled by the spike: `CAP_SYS_CHROOT` was not needed here (the
`sysusers` hook ran and created `polkitd` without it), but whether libalpm
skips `chroot(2)` for a root of `/` was not checked in its source, so a
package whose scriptlet fails without it may yet turn up. Whether any common
dependency needs `MKNOD` or `SETFCAP` (file capabilities, e.g. `iputils`) is
untested too. `setpriv --bounding-set=-all` needs `SETPCAP`, so it was left
out rather than granting that (the move to a non-root uid already clears the
effective and permitted sets, and `no_new_privs` keeps them from coming back). All three are on `baseline`'s list and can be added if
a package needs one.

### Installing what makepkg asks for

`makepkg -s` installs dependencies through `run_pacman -S --asdeps <deps>`,
which prefixes `PACMAN_AUTH` when it is set (`/usr/bin/makepkg`, lines
235-239 and 291 in pacman 7.1; `PACMAN_AUTH=()` is documented in
`/etc/makepkg.conf`). So the build can keep using `makepkg -s` with
`PACMAN_AUTH=(/aurcache/bin/aurcache-pod pacman-request)`: a client that sends
its argv over a unix socket to the root process, which runs pacman only for
the one shape makepkg uses -- `-S --asdeps` with `--noconfirm`,
`--noprogressbar`, `--needed`, then package names matching
`[a-z0-9@._+-]+` -- and refuses everything else (`-U`, paths, `--config`,
`--root`, `--dbpath`, `--overwrite`, hooks). That keeps makepkg's own
dependency rules (arch-specific and split-package dependencies, version
constraints) instead of reimplementing them, and it is option F's "root
helper with a fixed set of verbs" (`unprivileged-workers.md`) at its smallest.
The alternative, installing everything `.SRCINFO` names before the build, is
simpler and is the fallback if the helper's validation proves fragile.

What the root step still installs is chosen by the build: any package in the
job's sync repositories, including the AURCache repository. A hostile package
already published there runs its install scriptlet as root in the pod of
every build that depends on it. That is true of the chroot worker today, and
it is why user namespaces matter below.

---

## Where the code lives

### A. A Kubernetes backend inside the docker worker

The docker worker is "a container per build from an image" already, so it
looks like the natural host. Little of it carries over, though:

- `executor.rs` is bollard calls (pull, create, attach, wait, kill, remove)
  around bind mounts of host paths, which pods do not have; `network.rs` is
  Docker network plumbing; `config.rs` resolves the host-visible and
  container-visible paths of a shared directory. None of it applies to a pod.
- `commands.rs` is the reusable part (the keys, the config wrapping, the
  makepkg command), but it is built around `sudo pacman`, which the spike
  shows is exactly what a pod should not have.
- The crate is deprecated, and `executor.rs` in `aurcache-worker-core` is
  explicit that "each executor lives in its own crate with its own binary, so
  neither has to know the other exists and retiring one is a deletion rather
  than a refactor". A Kubernetes mode would tie the new path's life to the
  one being retired.

### B. The chroot worker's job logic, with a pod instead of nspawn

`aurcache-worker/src/job.rs` (1.26k lines) is the richest build flow, but it
is the pool, the caches and devtools all the way through: `Lease` from the
pool, `JobCaches` replicas, `SRCDEST` locks, cgroup placement, the
`aurcache-build` script, staging credentials for a host-side fetch. A pod
changes each of those, so what would be reused is shape, not code. The pieces
that are genuinely independent of nspawn are small and better moved to
`aurcache-worker-core` for both workers:

- `oom_reason` (the out-of-memory wording) and the parallelism rule;
- `built_sources::resolve` (VCS commits from a `SRCDEST`), which in a pod
  runs inside the pod;
- `credentials::augment_makepkg_conf` (`GIT_SSH_COMMAND` and
  `known_hosts`).

### C. A dedicated crate

`aurcache-worker-kube`, a binary on `aurcache-worker-core` like the other two,
plus a small agent binary that runs inside the build pod. It reuses from core,
unchanged: enrollment, the `Runner` (claim within concurrency, heartbeat,
self-abort, one terminal report), `BuildLog`, `upload_artifacts` (plus a
streaming variant), `report::ended`, `Timeout`/`StopWatch`,
`repo::append_to_pacman_conf`, `artifacts::is_artifact` and
`extract_source`.

**Recommendation: C.** Rough size, tests included:

| part | lines |
|---|---|
| dispatcher: config and declared settings | 250 |
| pod rendering (pure: job and config in, `Pod` out) | 350 |
| lifecycle: create, watch through a reflector, exec in and out, delete, sweep, readiness | 500 |
| log pump, status-to-report mapping | 250 |
| pod agent: root step, pacman helper, drop, ssh-agent, keyring, report | 700 |
| tests (rendering, mapping, helper argv validation, kube mock) | 600 |
| moved to core from the chroot worker | 150 |
| **total** | **about 2.8k** |

---

## Proposal

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

The **dispatcher** is a normal AURCache worker: it enrolls, registers with
`KIND = "kubernetes"`, claims jobs and heartbeats. Its `run_job` creates a pod
for the job, feeds it, follows it and turns how it ended into the report. It
never builds, needs no capabilities, and passes `restricted` itself. The pods
never see its key.

**One replica, `strategy: Recreate`.** Two processes with the same identity
would each list only their own builds in their heartbeats, and each heartbeat
abandons whatever the other holds. A rolling update does exactly that for a
moment. More capacity means a higher `WORKER_CONCURRENCY` or a second
dispatcher with an identity of its own, never `replicas: 2`.

**Its identity** lives where the other workers keep theirs, in its data
directory, so on a small `ReadWriteOnce` PVC. Supplying a key from a `Secret`
instead (so the pod needs no volume) is an open question.

### Concurrency and `ready_for_work`

`WORKER_CONCURRENCY` is the most build pods at once; the runner's gate already
enforces it. The question `ready_for_work` answers is different: whether the
cluster can start one more *now*, because a claimed job holds a lease the
server expects progress on (`executor.rs`, `ready_for_work`). The dispatcher
keeps a `kube::runtime::reflector` store of its own pods (a single watch, no
API call per check) and answers no while:

- any of its pods is `Pending` with `PodScheduled=False`, reason
  `Unschedulable`: the cluster is full, and another claim would only queue a
  second job behind it;
- the namespace's `ResourceQuota`, when there is one, has no room for one
  more build's requests (read from `status.hard`/`status.used`).

A pod that stays unschedulable past `WORKER_KUBE_SCHEDULE_TIMEOUT` (default
10 minutes) is deleted and reported `Interrupted` ("no node could take the
build"), so the server requeues it, possibly to another worker. The build
timeout counts from the claim, as it does on every worker, so time spent
pending is time the build does not get; that is the honest accounting, since
the server's job timeout is about how long a build holds its lease.

### Arches

The advertised arch list is configuration (`WORKER_ARCHES`), as on every
worker, not discovery. Each job's pod gets
`nodeSelector: {kubernetes.io/os: linux, kubernetes.io/arch: <arch>}`, mapped
as the docker worker's `docker_arch` maps (`x86_64` → `amd64`, `aarch64` →
`arm64`, `armv7h` → `arm`, `riscv64` → `riscv64`; the `arm` label carries no
variant). When the dispatcher is also granted `list` on nodes (an optional
ClusterRole), it checks at startup that a node exists for each arch it
advertises and warns otherwise; it never narrows the list on its own, since a
node pool scaled to zero is still capacity. Emulated builds (binfmt on the
nodes) stay the operator's business: the protocol carries one arch list and
nothing about how a worker builds it (`RegisterRequest::arches`).

Extra scheduling (`tolerations`, a node affinity for dedicated build nodes, a
`priorityClassName`, a `runtimeClassName` such as gVisor or Kata) is passed
through from configuration as a pod-spec fragment.

### Restarts and orphans

When the dispatcher restarts, its first heartbeat lists nothing, and the
server abandons every build it held. The pods those builds ran in are then
orphans, and the dispatcher deletes them before the runner starts: one
`DELETE` by label selector `aurcache.io/dispatcher=<fingerprint>`. That loses
the progress of builds that were running, which is what every worker does on
a restart today.

The pods also bound themselves without the dispatcher:
`activeDeadlineSeconds` is the build's timeout plus a grace, so a pod outlives
a dead dispatcher by at most that. Owner references would collect them too,
but an owner must be in the pod's own namespace, and the builds' namespace is
deliberately not the dispatcher's.

*Adopting* running pods instead (store the `JobDescriptor` in an annotation,
list the pods before the first heartbeat, resume following them) needs no
protocol change, since the first heartbeat would then list them and renew
their leases. It needs a way to hand the runner jobs it did not claim, which
it does not have. Worth it for long builds; deferred.

### Cancellation and timeouts

The runner trips the job's `CancellationToken` for a heartbeat `cancel` and
for the timeout, as for every executor. The dispatcher then deletes the pod
with a short grace period (`gracePeriodSeconds: 5`) and reports `canceled` or
the timeout through `report::ended`, as the docker executor does with
`kill_container`.

### The build pod

```yaml
apiVersion: v1
kind: Pod
metadata:
  name: aurcache-build-<build_id>
  namespace: aurcache-builds
  labels:
    app.kubernetes.io/managed-by: aurcache-worker-kube
    aurcache.io/dispatcher: <short fingerprint>
    aurcache.io/build-id: "<build_id>"
    aurcache.io/arch: <arch>
spec:
  restartPolicy: Never
  activeDeadlineSeconds: <timeout + grace>
  serviceAccountName: aurcache-build        # bound to nothing
  automountServiceAccountToken: false
  enableServiceLinks: false
  hostUsers: false                          # WORKER_KUBE_USER_NAMESPACES, see below
  nodeSelector: {kubernetes.io/os: linux, kubernetes.io/arch: <arch>}
  securityContext:
    seccompProfile: {type: RuntimeDefault}
  initContainers:
  - name: stage                             # receives the job, then exits
    image: <dispatcher image, by digest>
    command: [/usr/bin/aurcache-pod, stage, --into, /work]
    securityContext: &unprivileged
      runAsUser: 65534
      runAsNonRoot: true
      allowPrivilegeEscalation: false
      readOnlyRootFilesystem: true
      capabilities: {drop: [ALL]}
    volumeMounts: [{name: work, mountPath: /work}, {name: tools, mountPath: /tools}]
  containers:
  - name: build
    image: <builder image>
    command: [/aurcache/bin/aurcache-pod, build, --job, /work/in/job.json]
    securityContext:
      runAsUser: 0
      allowPrivilegeEscalation: false
      capabilities:
        drop: [ALL]
        add: [CHOWN, DAC_OVERRIDE, FOWNER, FSETID, SETUID, SETGID, KILL]
    resources:
      requests: {cpu: <build_cpus>, memory: <build_memory_max>, ephemeral-storage: <build_disk_max>}
      limits:   {cpu: <build_cpus>, memory: <build_memory_max>, ephemeral-storage: <build_disk_max>}
    volumeMounts:
    - {name: work, mountPath: /work}
    - {name: tools, mountPath: /aurcache/bin, readOnly: true}
    - {name: credentials, mountPath: /aurcache/credentials, readOnly: true}   # when configured
  - name: hold                              # keeps the pod up for the hand-back
    image: <dispatcher image, by digest>
    command: [/usr/bin/aurcache-pod, hold]
    securityContext: *unprivileged
    resources: {requests: {cpu: 10m, memory: 16Mi}, limits: {memory: 64Mi}}
    volumeMounts: [{name: work, mountPath: /work, readOnly: true}]
  volumes:
  - name: work
    emptyDir: {sizeLimit: <build_disk_max>}
  - name: tools
    emptyDir: {sizeLimit: 64Mi}
  - name: credentials                       # when configured
    secret: {secretName: <operator's>, defaultMode: 0400}
```

Notes on the choices:

- **The agent comes from the dispatcher's image**, copied into `tools` by the
  `stage` init container. The builder image is then the stock
  `aurcache-builder-v2`, untouched, and the agent is always the dispatcher's
  own version, so the two cannot drift. Its `sudo` grants are inert under
  `allowPrivilegeEscalation: false`.
- **The build user is the agent's**, created at start with a fixed uid
  (1001 in the spike), not the image's `ab`, whose uid is whatever
  `useradd --system` picked when the image was built (968 in the published
  one).
- **No `fsGroup`**: with one, Secret files become readable by that group, and
  the credential must be readable by root alone.
- **`hold`** is a regular container rather than a native sidecar, because a
  native sidecar is stopped as soon as the build container exits, and the
  dispatcher needs the pod up after the build to take the packages out. The
  dispatcher deletes the pod when it is done.

### Inside the build container

`aurcache-pod build`, as PID 1 and root:

1. Write `/etc/pacman.conf` (the job's, with the repository section appended
   by `repo::append_to_pacman_conf`) and the mirrorlist, checked as
   `build-runner.md` checks them (no `CacheDir`, `DBPath`, `RootDir`,
   `HookDir`, no `file://`). Run `pacman-key --init`, which is what
   `builder-entrypoint.sh` does today, then `pacman -Syu`.
2. Create the build user, hand it `/work/build`, `/work/src`, `/work/pkg` and
   its home, and open the pacman-request socket in a directory it can reach.
3. As the build user, via `setresgid`/`setgroups`/`setresuid` with
   `PR_SET_NO_NEW_PRIVS` already set by the runtime, import the job's
   `validpgpkeys` into the build user's keyring, start `ssh-agent` with the
   configured key when there is one (see "Credentials"), and run
   `makepkg -s` with the job's flags, `SRCDEST`, `BUILDDIR` and `PKGDEST`
   under `/work`, and `PACMAN_AUTH` on the helper. Its output goes to a log
   file in a root-owned directory and to stdout.
4. When makepkg exits, kill whatever the build left running: as PID 1,
   `kill(-1, SIGKILL)` reaches every other process in the container's PID
   namespace. Then, as the build user, resolve the VCS commits
   (`built_sources::resolve` over `/work/srcdest`; as the build user because
   git on a build-written repository runs hooks and config the build chose).
5. Write the report to `/dev/termination-log` (the kubelet keeps up to 4096
   bytes of it in the container's terminated state): exit code,
   `memory.peak` and `memory.events` `oom_kill` from the container's own
   cgroup, bytes under `/work`, VCS commits. Last, after step 4, so nothing
   the build left behind can write it.

### Getting things in and out

The pod needs the job in (source archive, configs, keys) and packages out,
and must never hold anything that lets it act as the worker. Three ways:

**Through the API, with `pods/exec` (proposed).** The dispatcher execs
`aurcache-pod receive` in the `stage` container and streams a tar of the
job's inputs to its stdin (the agent unpacks it with `extract_source`'s
rules), then the container exits and the build starts. After the build, it
execs `aurcache-pod send` in `hold`, which writes a tar of the regular files
in `/work/pkg` that pass `is_artifact`, opened `O_NOFOLLOW`, to stdout; the
dispatcher streams each entry to `upload_artifact` under its own identity. No
listener, no Service, no token: the pod reaches nothing in the cluster, and
its NetworkPolicy can say so. The cost is `create` on `pods/exec` in the
builds' namespace, and that packages travel through the API server: fine for
most, unproven for the 20 GiB-plus packages `MAX_ARTIFACT_SIZE` allows. The
first spike in "Implementation order" measures it.

**Push to the dispatcher, with a per-job token.** The dispatcher serves an
in-cluster endpoint; the pod gets a token for its own job only, held by root
in the build container (or by `hold`, which never runs build code), and
fetches its inputs and uploads its packages there. Faster for large
artifacts and needs no `pods/exec`, but it adds a listener, a Service, a
NetworkPolicy hole from the builds to the dispatcher, and a token whose
leaking is harmless only because it is per job: the build can always make
its own package hostile anyway, and the token reaches no other job.

**A per-job upload credential from the server.** An optional
`upload_token` in `JobDescriptor` and a route that accepts it would let the
pod upload straight to the server. That is within protocol 1's rules (an
optional field; a new route the worker uses only when the field is present),
but it is server work for something the two options above get without it,
and it puts the server's API in the builds' NetworkPolicy. Not proposed.

### Logs

The kubelet's log API (`get` on `pods/log`, `follow=true`) is the obvious
source, and the dispatcher would pump it into `BuildLog` as the docker
executor pumps `attach`. Two things argue against relying on it:

- following stops at log rotation on some kubelets (k3s-io/k3s#9060;
  OpenShift's OCPBUGS-38861, fixed in OpenShift's kubelet), and only the
  newest file is served, so a build past the kubelet's `containerLogMaxSize`
  (10 MiB by default) can lose lines;
- it gives no way to resume from a known point.

So the agent writes the build's output to `/work/log/build.log` as well, and
the dispatcher follows it with an exec of `aurcache-pod tail --from <offset>`
in `hold`, resuming from the byte it last forwarded if the stream breaks
(the kubelet's `streamingConnectionIdleTimeout`, 4 h by default, ends silent
sessions). The log API remains the fallback for what happens before the agent
opens its file (image pull failures are in the pod's events and status, not
its log).

### How a build ended

From the build container's `state.terminated` and the pod's `status`, through
the reflector:

| observed | report |
|---|---|
| `exitCode` from the agent's report | `report::ended(None, elapsed, Exit::Code(..))`, with `peak_memory_bytes`, `disk_usage.workdir` and `vcs_commits` from the termination message |
| `reason: OOMKilled` | failed, with `oom_reason`'s wording naming `build_memory_max` (on cgroup v2 the kubelet sets `memory.oom.group` on each container unless `singleProcessOOMKill` is set, so an OOM kills the whole container, the agent included, and leaves no report; Kubernetes blog, "The Shift to cgroup v2", 2026-10-06, and kubernetes/kubernetes#117793) |
| pod `reason: Evicted`, message about ephemeral storage | failed, "out of disk: the build passed its ephemeral-storage limit of N and was evicted" |
| pod `reason: DeadlineExceeded` | timed out (the backstop; the dispatcher's own timeout normally fires first) |
| unschedulable past the timeout, `ErrImagePull`/`ImagePullBackOff`, `CreateContainerConfigError` | `Interrupted`, with the event's message: the worker's problem, not the package's |
| the pod disappears (node lost, deleted by hand) | `Interrupted` |

The docker worker reports none of the measurements; this one reports all but
the chroot's share of disk (the container's writable layer is not visible from
inside, so `DiskUsage::chroot` is `None`).

### Credentials and PGP keys

**SSH build credentials.** The operator puts the key in a Secret, mounted
`0400` into the build container only. The agent, as root, reads it, starts
`ssh-agent` *as the build user* and loads the key into it through
`ssh-add -` on a pipe, then makepkg's `SSH_AUTH_SOCK` and `GIT_SSH_COMMAND`
point at it (`credentials::augment_makepkg_conf`). That is the property the
chroot worker gives (`aurcache-worker/src/agent.rs`: the agent runs as the
build user because `ssh-agent` checks the peer uid): the build can *use* the
key for the length of the build but cannot read it, since the file is
root's and `ssh-agent` marks itself non-dumpable. It is one key per
dispatcher, given to every build it runs, as one key per chroot worker is;
`WORKER_PACKAGES` routes the packages that need it. The generated-key
onboarding (`docs/docs/workers/credentials.md`) does not carry over: a key
generated in a pod dies with it, so the dispatcher could generate one into a
Secret it owns (RBAC on one named Secret), or the operator provides it. Open
question.

**PGP keys.** First version: the agent imports `validpgpkeys` from the
keyserver per build, as the docker worker does
(`commands.rs`, `fetch_required_pgp_keys_cmd`). Later, the dispatcher fetches
each key once over HKP (`/pks/lookup?op=get`) with its own HTTP client, keeps
it, and ships the armored keys with the job, so builds stop depending on the
keyserver being up. No shared writable keyring in any pod, which is what the
chroot worker's per-job replicas work hard to emulate.

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
sizes `MAKEFLAGS` as on the chroot worker (the parallelism rule moved to
core). No swap setting is declared.

**Persistent build directories** (`persistent_builddir`) are not honoured in
a first version: the build runs in a fresh `/work/build`, and the worker
reports a warning, as the chroot worker does when it cannot prepare one.

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
          podSelector: {matchLabels: {app: aurcache-server}}}]
    ports: [{protocol: TCP, port: 8081}]    # the repository, never the worker port
  - to:
    - ipBlock:
        cidr: 0.0.0.0/0
        except: [<pod CIDR>, <service CIDR>, <node CIDR>, 169.254.0.0/16]
    - ipBlock:
        cidr: ::/0
        except: [<IPv6 cluster ranges>, fe80::/10]
```

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
  tell from the API whether it is enforced. It can probe, though: at startup,
  a pod with the builds' labels that tries the server's worker port and the
  metadata address, and a warning, at error severity, when either answers.

### The build environment

The chroot worker upgrades its own base chroot and keeps the previous one; a
pod starts from whatever its image is. The options:

| | cost | freshness |
|---|---|---|
| the published `aurcache-builder-v2`, `pacman -Syu` at the start of every build | the upgrades since the image was built, downloaded by every build, since nothing is cached | always current; the docker worker already does this (`commands.rs:65`) |
| an image the dispatcher rebuilds in the cluster | a builder: kaniko is archived upstream (June 2025, forks continue), buildah needs user namespaces or privileges of its own | current to the last rebuild; a privileged-or-userns builder is what this design avoids |
| an image the operator's CI rebuilds and pushes on a schedule | outside AURCache | as fresh as the schedule, which is Lukas-Heiligenbrunner/AURCache#352's concern turned over to the operator |
| an init container that upgrades into a volume | the whole root on a volume, copied per build | no better than `-Syu`, much larger |

**Proposal: the published image plus `-Syu` per build**, with
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

**Proposal**: a setting, `WORKER_KUBE_USER_NAMESPACES`, off by default so the
worker runs on any cluster, with the docs recommending it on and the
dispatcher logging, at warning severity, that builds run without it. Whether
the root step works unchanged under it is the one part of the spike not yet
run in a user namespace; rootless Podman builds with `sudo` in
`unprivileged-workers.md` (option H) are the strongest evidence that it does.

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

## Testing without a cluster in CI

- **Unit tests** for the pure parts, which carry most of the risk: pod
  rendering (job and settings to `Pod`, including that nothing ever renders
  `privileged`, a `hostPath` or an `fsGroup`), the status-to-report mapping,
  and the pacman helper's argument validation (the security-critical piece,
  worth property tests against flags and names that must be refused).
- **The dispatcher against a mock API**: kube's client can be built over a
  `tower` service, so `tower-test`'s mock serves canned responses for create,
  watch, exec and delete (the approach kube documents for testing; the exec
  stream's framing over the mock is unverified).
- **The agent in a plain container**, with the pod's security context
  translated to flags exactly as in the spike's row G
  (`--user 0 --security-opt no-new-privileges --cap-drop=ALL --cap-add=...`),
  building `docker/testpkg/hello` and an adapted `hostile-fixture` that
  asserts it cannot `sudo`, cannot read the credential, and cannot write the
  termination message. This needs only Docker, so it fits beside
  `test-sandbox.sh`.
- **kind** for the integration: a server, the dispatcher and the build pods in
  one cluster, building the test package end to end. kind's nodes are
  privileged containers, but that only matters for what kind cannot show
  (the node boundary itself); the build pods inside are ordinary unprivileged
  pods under the same admission and the same kubelet, which is what is being
  tested. Its default CNI, kindnet, implements NetworkPolicy (kindnet's
  documentation; its repository was archived in October 2025, so pinning, or
  Calico, is the safer default), so the probe and a "build cannot reach the
  worker port" assertion run too. User namespaces in kind depend on the
  runner's kernel (6.3+; GitHub's Ubuntu 24.04 runners ship 6.8) and the node
  image's containerd (2.0+), both unverified for a specific kind release.
  k3s is the alternative where kind's privileged nodes are unwelcome.
- What CI cannot cover: managed offerings, eviction timing on a real disk,
  and artifact throughput through a production API server.

---

## What it costs operators

- **A namespace** for builds, labelled
  `pod-security.kubernetes.io/enforce: baseline`, and the dispatcher's
  namespace at `restricted`.
- **RBAC**, namespace-scoped in the builds' namespace:

  ```yaml
  rules:
  - apiGroups: [""]
    resources: [pods]
    verbs: [create, get, list, watch, delete, deletecollection]
  - apiGroups: [""]
    resources: [pods/exec]
    verbs: [create, get]        # get: the WebSocket upgrade on some API servers (KEP-4006); unverified per version
  - apiGroups: [""]
    resources: [pods/log, events, resourcequotas]
    verbs: [get, list, watch]
  ```

  plus, optionally, a ClusterRole with `list` on `nodes` for the arch check,
  and a ServiceAccount for builds bound to nothing.
- **A CNI that enforces NetworkPolicy**, or accepting that builds can reach the
  cluster.
- **Storage**: a small `ReadWriteOnce` PVC for the dispatcher's identity; a
  StorageClass only for the optional hardening and the later caches.
- **Nodes**: nothing. Standard `kubernetes.io/arch` labels; optionally a taint
  for dedicated build nodes.
- **Pulls**: the builder image (about 1 GB compressed) once per node, and the
  dispatcher's image for the agent.

### Managed offerings

- **GKE Autopilot**: "allows running as root to enable most workloads"; the
  default capability set includes all seven the build container adds; it
  blocks privileged containers and denies writable `hostPath`; `RuntimeDefault`
  seccomp is applied to all workloads (GKE Autopilot security measures page).
  So the pod as proposed should be admitted. User namespaces are not
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

## Implementation order

0. **Spike in kind**: the pod as specified above, the exec hand-in and
   hand-back (including a multi-GB artifact through the API server), the log
   tail across a kubelet log rotation, the OOM and eviction mappings, and row G
   repeated with `hostUsers: false`. Decide the artifact transport from it.
1. **Move to `aurcache-worker-core`**: `oom_reason`, the parallelism rule,
   `built_sources`, the credential `makepkg.conf` lines, and a streaming
   `upload_artifact` that takes one tar entry at a time.
2. **`aurcache-pod`**, the agent (`stage`, `receive`, `build`,
   `pacman-request`, `send`, `tail`, `hold`), with the plain-container test
   script in CI.
3. **`aurcache-worker-kube`**, the dispatcher: settings, rendering,
   lifecycle, sweep, readiness, the NetworkPolicy probe. `kube` 5.1 with the
   `client`, `runtime`, `rustls-tls` and `ws` features (exec needs `ws`),
   and `k8s-openapi` 0.29 with the oldest API feature it offers, `v1_33`, so
   the generated types cover clusters from 1.33 on. Its rustls provider has to
   match the workspace's.
4. **Manifests and docs**: namespace, RBAC, NetworkPolicy, Deployment;
   `docs/docs/workers/kubernetes.md`; the worker image gets the dispatcher
   binary.
5. **kind in CI**, beside the e2e suites, on the same "run when it changed"
   footing as `test-kernel.sh`.
6. **Later**: keys shipped by the dispatcher, `ReadWriteOncePod` source caches,
   the caching mirror's guidance, persistent build trees, adoption of running
   pods across restarts.

---

## Open questions

- Is `baseline` acceptable as the floor for the builds' namespace? `restricted`
  cannot run a build that installs dependencies (the spike), so the
  alternative is giving up `makepkg -s` altogether.
- User namespaces: off by default (runs anywhere) or on by default (refuses
  older nodes and runtimes)?
- Artifact transport: `pods/exec` through the API server (proposed, pending
  the large-artifact spike) or a push to the dispatcher with per-job tokens?
- The builds in a namespace of their own (no owner references, so cleanup
  relies on the sweep and `activeDeadlineSeconds`) or beside the dispatcher?
- The build credential: provided as a Secret only, or generated by the
  dispatcher into a Secret it owns?
- Ship without caches, and add the source cache first? Or is the VCS clone
  cost enough to hold the first release until `ReadWriteOncePod` caches
  exist?
- Is `persistent_builddir` unsupported (with a warning) acceptable for a first
  version, given that the packages which ask for it are the largest ones?
- Should the docker worker and `build-runner.md`'s in-container script also
  adopt the root-step-then-drop model and the pacman helper? The spike ran on
  the stock builder image, so the change is the command, not the image.
- Adoption of running build pods across a dispatcher restart: worth a runner
  change in `aurcache-worker-core`?
- Per-arch concurrency (a cluster with many x86_64 nodes and one aarch64
  node), or one total, as every worker has today?
