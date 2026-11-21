# Building in Kubernetes, one pod per build

Status: **Built, not yet merged** (branch `feat/kubernetes-executor`) · Last
updated: 2026-11-21 · Kubernetes facts checked against the v1.36
documentation; container spikes run on Docker, kernel 7.2; the worker run in
kind v0.33 (Kubernetes 1.37, one node, kindnet)

A worker that builds each job in a short-lived pod of its own, with no
privileged pod and no kernel or filesystem requirement on the nodes. It is
the unprivileged alternative to running the chroot worker as a privileged
StatefulSet, which stays the full-featured way to run on Kubernetes.

This document keeps the decisions, what was set aside, and what is open.
How it works is in `container-workers.md` (the build agent, shared with the
Docker worker), the module docs of `aurcache-build-agent`'s `agent/transfer`
(what each `exec` into a build pod carries) and
`docs/docs/workers/kubernetes.md` (manifests, variables, RBAC, the
NetworkPolicy, troubleshooting).

Related:

- `design/proposed/unprivileged-workers.md`: the threat model ("What counts
  as contained") and options A to H, of which this is a cluster-shaped G/H.
- `design/proposed/signed-repository.md`: worker approval is the real trust
  decision.
- `design/in-progress/worker-protocol-versioning.md`: why nothing here may
  change protocol 1 except by adding optional fields. Nothing here does.
- Upstream issues Lukas-Heiligenbrunner/AURCache#276 (running without
  `--privileged`) and #352 (refreshing the builder image locally).

---

## The idea

The chroot worker needs root mostly to keep concurrent builds on one machine
apart. With one pod per build, Kubernetes provides most of that itself:

| chroot worker | in a pod |
|---|---|
| btrfs snapshot of a refreshed base chroot | a new container from the builder image; freshness is the image's problem |
| per-build qgroup | `ephemeral-storage` and `emptyDir.sizeLimit`, enforced by eviction |
| per-build cgroup | the container's own, `resources.limits` |
| `systemd-nspawn` | the container runtime; no mounts inside the pod |
| build firewall by uid | a NetworkPolicy, where the CNI enforces it |
| Landlock around the host-side fetch | not needed: the fetch runs in a pod that holds nothing else |
| caches, kept failed chroots | none in a first version |
| `sudo pacman` | a root step, then a drop to an unprivileged user |

---

## Decisions

### Pod Security `baseline` for builds

A spike, with Docker flags standing for pod fields, settled what a build
container needs. `sudo` fails with all capabilities dropped and under
`no-new-privileges`; root with no capabilities cannot set package ownership
or switch to a build user. Root with `CHOWN`, `DAC_OVERRIDE`, `FOWNER`,
`FSETID`, `SETUID`, `SETGID` and `KILL`, and
`allowPrivilegeEscalation: false`, installs the dependencies and then drops
to an unprivileged uid with no capabilities and `no_new_privs`.

`restricted` requires a non-root user and allows no capability but
`NET_BIND_SERVICE`, with or without user namespaces, so it cannot run a
build that installs dependencies. The seven are on `baseline`'s list, and
the pod meets every other `restricted` control. The build code has no
capability, no setuid path to root and no `sudo`, which is stronger than the
chroot worker. What the root step installs is still the build's choice
(the `.SRCINFO` the build user prints, `container-workers.md`): a hostile
package in the job's repositories runs its scriptlet as root in the pod,
before the build. That is why
user namespaces matter, and why the SSH key never enters the pod.

### A crate of its own

`aurcache-worker-kube`, on `aurcache-worker-core` like the other workers.
Set aside: a backend in the Docker worker, whose code is bind mounts of host
paths, which pods do not have; and the chroot worker's job logic, which is
the pool, the caches and devtools throughout.

### One dispatcher, builds in a namespace of their own

The dispatcher is the worker: one approved identity that claims, heartbeats
and reports. It never builds and passes `restricted`. Build pods never reach
the worker port and hold no credential. They run in their own namespace at
`baseline`, so the dispatcher's is not lowered by what builds need.

**One replica, `strategy: Recreate`.** Two processes with one identity each
list only their own builds, and each heartbeat abandons what the other
holds. More capacity is a higher `WORKER_CONCURRENCY` or a second
dispatcher with its own identity.

**One builds namespace per dispatcher.** A Role cannot scope `pods/exec` by
label, so another dispatcher there could exec into this one's pods and use
its SSH agent; `create pods` reaches every Secret in the namespace; and the
startup sweep takes every build pod in it.

### Claiming only what the cluster can start

A claimed job holds a lease the server expects progress on. So the
dispatcher claims nothing while any of its pods is unschedulable and the
server may hand out another build of that pod's architecture, since another
claim would only queue behind it, or while a server-side dry run of the pod
it would create is refused: a job claimed into a cluster that refuses its
pod would spend an attempt for nothing. Any answer the API server gives is
taken as its answer for a real pod, and no answer holds claims back too: an
API server away for minutes would otherwise spend every attempt of every
package offered meanwhile.

A pod without a node after `WORKER_KUBE_SCHEDULE_TIMEOUT` is reported
`Interrupted`, so another worker may take the build. A scheduled pod gets
the same allowance to start, since a volume or network that cannot be set
up shows only in events and would otherwise be blamed on the package.

### Architectures

The arch list is configuration (`WORKER_ARCHES`), not discovery. The
dispatcher refuses an architecture its own image is not built for, since
every pod's agent comes from it, and never drops one for good: a node pool
scaled to zero is still capacity. A claim cannot name an architecture
(protocol 1 carries the list at registration only), so a pod waiting for a
node would hold back every architecture's builds. Instead the executor tells
the runner which architectures it serves now (`Executor::serving`), and the
runner registers again when that changes, as it does for a changed
concurrency. An architecture with a pod no node takes is left out until one
schedule timeout after its last such pod; once registered without it, that
pod holds no claim back. With nothing left to serve (one architecture, or
all of them waiting), the list stays whole and waiting pods hold claims
back. The cost is one build of that architecture handed back per schedule
timeout and cool-down while it has no node, instead of every other
architecture stopped. The operator's pod template is read as scheduling
fields only, so it cannot add a volume, a container or a security context.

### Restarts, stopping and orphans

Running pods are **not adopted** across a restart: the first heartbeat
lists nothing, so the server has already abandoned their builds, and the
runner has no way to take a job it did not claim. **At startup** the
dispatcher deletes every pod labelled
`app.kubernetes.io/managed-by: aurcache-worker-kube` in its builds
namespace, whichever dispatcher made it, so that the pods of an identity
lost with its volume are collected too. **Told to stop**, it reports its
builds `Interrupted`, so the server queues them again at once, and deletes
only its own pods (its `aurcache.io/dispatcher` label), never another
dispatcher's. **Before each claim** it deletes its own pods that no build
holds, which would otherwise count against a quota until their deadline.

That deadline is the build's timeout plus five minutes, the server's
`REAP_BACKSTOP_GRACE`, after which it refuses what the worker sends. No
delete uses a grace of zero, which would leave the API before the node has
confirmed the containers gone.

### The build pod

- **The agent comes from `WORKER_KUBE_AGENT_IMAGE`**, required, which the
  manifests set to the dispatcher's own image; the builder image stays the
  stock `aurcache-builder-v2`. Reading the image from the dispatcher's own
  pod was set aside: it takes `get` on every pod of its namespace, the
  server's included. With a tag that moves, a node may pull another build
  of the agent than the dispatcher's; the agent refuses a job of another
  interface number, which keeps the two honest.
- **`hold` is a regular container**: a native sidecar is stopped when the
  build exits, before the packages are out. It runs no build code.
- **The report is a file**, not the termination message, which the kubelet
  cuts at 4096 bytes.
- **Nothing that grants anything is mounted**: no Secret, no ServiceAccount
  token, no service links.

### Getting things in and out through `pods/exec`

The job goes in and the packages come out over `exec`: no listener, no
Service, no token, and the NetworkPolicy can say the pod reaches nothing in
the cluster. The cost is `create` on `pods/exec` and packages travelling
through the API server.

**In.** `receive`, exec'd in `stage`, writes its pid into `in/ready`.
`stage` exits only once `in/ready` exists and that process has exited and
been reaped, at most 30 s later, so the container does not end, and take
`receive` with it, before `receive` is done. The dispatcher's memory is about
15 MiB of its own plus a source per running build, held until its pod has it;
a source is held twice for a moment as its download completes, so the worst
case, every download completing at once, is twice `WORKER_CONCURRENCY` times
the largest source, and (`WORKER_CONCURRENCY` + 1) times is the usual
figure.

**Out, streamed.** The dispatcher has no disk for packages, so each file
goes from a `send` exec straight into the upload's request body. It counts
when the server accepted it, the exec ended cleanly and the byte count
matches the manifest. There is no digest: nothing runs in the build
container by then, and every hop is a byte stream whose failures the count
and the exec status catch. The status alone does not: the API server can
answer `Success` for a stream cut short (client-go reports no error when the
error stream ends empty, and the API server's stream translator then writes
`Success`), so a successful exec says the process exited 0, not that its
output arrived whole, and the count is what says the file did. The artifact
route already takes one streamed file per request, so this needs no protocol
change.

Set aside: one tar of every package through one exec, which the dispatcher
would have to cut into request bodies and could not resend one file of; a
push from the pod to the dispatcher, which adds a listener, a Service and a
NetworkPolicy hole for a relay no shorter; and, for now, an upload straight
from the pod ("Later").

### Logs and outcomes

The dispatcher follows the agent's log file with a `tail` exec, resuming
from the last byte forwarded. A `tail` that succeeded may have been cut short
too, so the log is read to its end only once a further `tail` from the last
byte has nothing to send: one exec more per build. The kubelet's log API was
set aside: following stops at rotation on some kubelets (k3s-io/k3s#9060,
OCPBUGS-38861), only the newest file is served, and it cannot resume from a
known point.

What is the package's doing fails the build: makepkg's exit, out of memory,
an eviction for passing its own disk limit. What is the cluster's or the
worker's is `Interrupted`, so the build is queued again.

### The SSH build credential stays in the dispatcher

The key is in a Secret mounted into the dispatcher alone, which loads it
into an `ssh-agent` per build that the build reaches over an exec stream: a
build can use the key while it runs, and never obtains it. Mounting it into
the build pod was set aside: a key kept from the build user is not kept
from root in the pod, where a dependency's scriptlet runs, and copying it
out that way was shown to work. `known_hosts` is required, because every
pod is new and an ssh that trusts on first use would trust any host every
time.

PGP keys come from the keyserver per build, into the build user's own
keyring, so no keyring is shared.

### Limits are best effort

Limits are the server's recommendation, applied as well as Kubernetes
allows and never refused for being imperfect. The disk limit evicts some
seconds after a build passes it rather than refusing the write: weaker than
a qgroup, and accepted. Memory and CPU requests equal their limits, so a
build gets what it is told it has. The worker-wide total is a
`ResourceQuota`, which the dry run honours. Persistent build directories
are not supported, since a pod's volume goes with the pod; a job that asks
for one gets a fresh tree and a debug line only, because the setting is on
by default.

### The network: one NetworkPolicy

Builds get no ingress, DNS, the server pod's repository ports but not its
worker port, and the internet outside every private and link-local range.
A build that reaches the worker port can submit a registration. Excluding
every private range rather than one cluster's needs no knowledge of the
cluster, and keeps builds off the API server and the cloud metadata
endpoint, which matters most.

Whether the CNI enforces the policy is not in the API, so the documentation
says how to check by hand. A CNI may apply it some seconds after a pod
starts (kindnet, about ten); a build pod spends those seconds waiting for
the job and in the agent's root step, before any package code runs.

### The build environment and caches

Builds use the published image, upgraded per build: correct by
construction, at a cost in bandwidth that a pacman caching proxy in the
cluster removes. The proxy caches the official repositories only, never
AURCache's, whose packages can change bytes under one filename
(`stale-shared-pacman-cache.md`). Set aside: an image rebuilt in the
cluster (kaniko is archived; buildah needs privileges of its own), by the
operator's CI (outside AURCache), or by an init container into a volume (no
fresher, much larger).

There are no caches in the first version. `hostPath` caches are ruled out:
they break the isolation that is the point, `baseline` forbids them, and a
cache shared by concurrent builds brings back the races the chroot worker's
cache code avoids, as `ReadWriteMany` does across nodes.

### User namespaces: off by default, the operator's to turn on

With `hostUsers: false`, root in the pod is an unprivileged range on the
node. It needs Linux 6.3, idmap-capable filesystems and a recent runtime, so
it is off by default and the dispatcher warns without it.
`WORKER_KUBE_USER_NAMESPACES` is read from the dispatcher's environment, not
declared as a setting, since a server that could set it could turn it off.
An API server with `UserNamespacesSupport` off drops the field silently, so
the dry run and every created pod must come back with it. Nothing else
needs a kernel feature past what Kubernetes needs.

---

## Threat model

A compromised dispatcher can create only pods the builds' namespace admits,
which `baseline` keeps from being privileged or mounting host paths: it is
not node root, where a compromised chroot worker is host root. Root code in
a build pod has no host path, `CAP_SYS_ADMIN` or token; an escape needs a
kernel or runtime bug, and lands as the node's uid 0, or as an unmapped uid
with user namespaces. Weaker than the chroot worker: a disk limit that
evicts, a network rule the worker cannot verify, no user namespaces by
default. Stronger: no privileged container, build code with no path to
root, no shared cache or keyring to poison.

## Later

In rough order of worth: `ReadWriteOncePod` source caches per pkgbase,
which make the shared-`SRCDEST` race impossible by construction; keys
fetched once by the dispatcher and shipped with the job; guidance for the
caching mirror; uploading straight from the pod; persistent build trees on
the source caches' volumes; adopting running pods; the dispatcher's key from
a Secret; a startup check that each advertised architecture has a node,
which needs `list` on nodes, a ClusterRole.

**Uploading straight from the pod**, if the relay proves too slow, would
take a per-job upload grant (optional fields within protocol 1) handed to
`hold` after the build exited. The cost is the network: a NetworkPolicy is
per pod, so `hold` reaching the worker port means the build reaching it,
and it needs an upload listener of its own on the server.

## Still open

- `CAP_SYS_CHROOT`, `MKNOD`, `SETFCAP`: not granted, not yet needed. A
  package whose scriptlet wants one will fail; all three are on
  `baseline`'s list.
- Whether quota admission runs in a dry run on every supported version, as
  the readiness check assumes. It does on 1.37.
- **User namespaces under the root step have not run anywhere.** The kind
  node on the development machine cannot start a pod with
  `hostUsers: false`.
- **One node, one architecture.** Nothing has run on several nodes, where
  the agent image is pulled per node, or on arm.
- Throughput through a production API server, and how proxies in front of
  one treat a long exec; whether `pods/exec` needs `get` for the WebSocket
  upgrade on every version (KEP-4006).
- Managed offerings: GKE Autopilot should admit the pod; EKS Fargate's
  NetworkPolicy enforcement and start-up latency are unverified. Whether
  pacman and makepkg run under gVisor or Kata is untested.
- CI has not run the kube-worker image's publishing step, which is new.
