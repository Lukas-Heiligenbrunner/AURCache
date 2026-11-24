---
sidebar_position: 3.5
---

# Kubernetes worker

The Kubernetes worker builds each package in a pod of its own. It needs no
privileged pod, no particular kernel and no access to a node: it runs on a
cluster that admits ordinary workloads, including one that enforces the
`baseline` Pod Security level.

To AURCache it is one worker. A small **dispatcher** Deployment enrolls, is
approved, claims builds and reports them. For each build it creates a pod in a
namespace of its own, feeds it the job, follows its log, takes the packages out
and deletes the pod. The dispatcher never builds, and a build pod never talks to
the server's worker port, never sees the dispatcher's key, and holds no
credential for the Kubernetes API.

## When to choose it

| You have | Use |
|---|---|
| A Linux 6.7+ host with btrfs, where a privileged container is acceptable | the [chroot worker](./split-chroot.md): clean `devtools` chroots, [caches](./storage-pool.md), kept build trees, kernel-enforced disk quotas |
| A host with Docker or Podman and nothing else | the [Docker worker](./docker.md) |
| A Kubernetes cluster, and no wish to run a privileged pod or to depend on a node's kernel | this worker |

Choose it when builds should use a cluster's spare capacity, when the cluster
forbids privileged pods, or when the nodes are not yours to configure. Choose
the chroot worker when the same packages are rebuilt often and their
dependencies and sources are large: this worker has no caches, so every build
downloads both again.

## What the cluster needs

- **No privileged pods and no kernel floor.** The build container runs as root
  inside the pod with `allowPrivilegeEscalation: false`, all capabilities
  dropped except `CHOWN`, `DAC_OVERRIDE`, `FOWNER`, `FSETID`, `SETUID`, `SETGID`
  and `KILL`, and the `RuntimeDefault` seccomp profile. It uses no `hostPath`,
  host network or host PID, and asks nothing of the nodes.
- **A namespace for the builds** (`aurcache-builds` by default) at the Pod
  Security `baseline` level. The dispatcher's own namespace needs nothing of the
  sort: the dispatcher passes `restricted`.
- **A CNI that enforces NetworkPolicy** (Calico, Cilium, kube-router, kindnet;
  not flannel alone). The manifests firewall build pods with a NetworkPolicy,
  and without enforcement that policy does nothing. See
  [What a build can reach](#what-a-build-can-reach).
- **RBAC**, namespace-scoped. In the builds' namespace the dispatcher may
  create, list, watch and delete pods (and delete them by label), open
  `exec` sessions into them and read their logs. It holds no role in its own
  namespace, and no cluster-wide one.
- **A small volume** for the dispatcher's identity: a 1 Gi `ReadWriteOnce`
  claim from the default StorageClass, or one you name. Packages stream through
  the dispatcher and are never stored.
- **Nodes labelled `kubernetes.io/arch`** (every node is) for each architecture
  you build for.
- **Image pulls**: the builder image (about 1 GB compressed) once per node, and
  the dispatcher's image on every node that builds: two of a build pod's three
  containers run from it, one to install the build agent and take the job in,
  one to hand the packages back.

## Installing

Apply the server alone first, then the worker, from the repository root:

```bash
kubectl apply -k kubernetes/server/
kubectl apply -k kubernetes/kube-worker/
```

`kubectl apply -k kubernetes/` is not the first step here: it deploys the
server together with a privileged [chroot worker](../setup/kubernetes.md) and
raises the `aurcache` namespace to the `privileged` Pod Security level, which
this worker exists to avoid. To install from that file instead, comment out
`components/chroot-workers` in `kubernetes/kustomization.yaml` and uncomment
the `kube-worker` line there. The two kinds of worker can also run side by
side.

The worker is a kustomization of its own because its resources live in two
namespaces and name them. It expects the namespace `aurcache` and the server's
Service `aurcache` in it, as `kubernetes/server/` creates them, and creates:

| Resource | Where | Purpose |
|---|---|---|
| Namespace `aurcache-builds`, labelled for Pod Security `baseline` | cluster | where build pods run |
| ServiceAccount for the dispatcher, with a Role and RoleBinding | `aurcache`; the Role in `aurcache-builds` | the permissions above |
| ServiceAccount `aurcache-build` | `aurcache-builds` | what build pods run as; bound to nothing, with no API token mounted |
| NetworkPolicy | `aurcache-builds` | the firewall around build pods |
| Deployment `aurcache-kube-worker` and its volume | `aurcache` | the dispatcher: one replica, strategy `Recreate`, unprivileged, a read-only root filesystem |
| ResourceQuota | `aurcache-builds` | an optional ceiling on all builds together; not applied unless enabled in `kustomization.yaml` |

Render the result without a cluster with
`kubectl kustomize kubernetes/kube-worker/`.

The dispatcher's image is
`ghcr.io/lukas-heiligenbrunner/aurcache-worker-kube`, published for `amd64`,
`arm64` and `arm/v7`: `latest` and a tag per version from releases, `git` from
the development branch. The manifests name `git` and pull it at every start,
as the server's manifests do, because no release contains this worker yet. So
until you pin it, any restart of the worker is an upgrade to that day's
`git`. Pin what you run by digest in `kubernetes/kube-worker/kustomization.yaml`
(`images:` is there, commented), and a release the same way once one exists,
the same version as the server's; [Image versions](../setup/kubernetes.md#image-versions)
has the reasons and the commands.

It is also the image of the agent that runs in every build pod. The manifests
name it a second time for that, in `WORKER_KUBE_AGENT_IMAGE`, and the
kustomization copies the Deployment's image into that variable, so setting the
image there moves both. An overlay of your own that changes the image has to
change the variable too. With a tag that moves (`git`, `latest`) a node may
pull a newer or older agent than the dispatcher that hands it a job; the two
check that they speak the same interface, and a build whose agent does not is
handed back. A version tag or a digest makes them the same build.

### Approving

A new worker is `pending` on the Workers page and cannot build until approved,
as for every worker (see [Managing workers](./managing.md#approving)). To have
it approved on its own, create the `aurcache-enrollment` Secret the server
reads, in the `aurcache` namespace, before you apply the worker:

```bash
kubectl -n aurcache create secret generic aurcache-enrollment \
  --from-literal=token="$(openssl rand -hex 32)"
```

The dispatcher reads the same Secret and presents the token when it enrolls. It
shows as `kubernetes` in the Workers page's **Type** column.

### One replica only

Never raise `replicas` above 1. A worker is known by its key, and two pods with
one key each report only their own builds, so each report abandons what the
other holds. A rolling update does the same for a moment, which is why the
Deployment uses `Recreate`. For more builds at once, raise
`WORKER_CONCURRENCY`. For a second worker (other architectures, its own
credential), copy `worker.yaml` under another name: it enrolls as a worker of
its own.

Give each dispatcher a builds namespace of its own (`WORKER_KUBE_NAMESPACE`,
with the namespace, the builds' ServiceAccount, the Role and RoleBinding, the
NetworkPolicy and the quota copied under it). Kubernetes cannot limit a Role
to pods carrying a label, so two dispatchers in one builds namespace may each
`exec` into the other's build pods, and so use the SSH key the other has
while a build of its runs. A dispatcher also deletes every build pod in its
builds namespace when it starts, whichever dispatcher made it, so that the
pods of one whose identity was lost (its volume replaced) are collected; a
second dispatcher there would end the first one's builds each time it
starts. Keep nothing else in a builds namespace either: whoever may create
pods in a namespace can mount any Secret in it.

### Stopping and updating

When its pod is deleted or its Deployment updated, the dispatcher receives
`SIGTERM`, claims nothing more, deletes the pods of the builds it is running,
and reports each of those builds to the server as interrupted; the server
ends them and queues a new build of each package at once. Last, it deletes
any pod of its own still there; another dispatcher's are left alone. This
takes a few seconds of the pod's 60-second grace period. The builds' progress
is lost: nothing adopts a running build pod.

## Configuration

The dispatcher starts from environment variables, which the manifest's
`worker.yaml` sets. Beside the ones every worker reads (`AURCACHE_URL`,
`WORKER_DATA_DIR`, `WORKER_NAME`, `WORKER_ARCHES`, `AURCACHE_REPO_URL`, and the
settings described in [Worker Configuration](./split-chroot.md)), it reads the
following.

### Where builds run, and with what

These stay on the machine: AURCache cannot change them from the Workers page,
because they decide what a build may reach.

| Variable | Default | Description |
|---|---|---|
| `WORKER_KUBE_NAMESPACE` | `aurcache-builds` | Namespace build pods are created in. The Role and NetworkPolicy in the manifests are for this namespace: change them together. |
| `WORKER_KUBE_BUILDER_IMAGE` | `ghcr.io/lukas-heiligenbrunner/aurcache-builder-v2:latest` | The image each build runs in. |
| `WORKER_KUBE_IMAGE_PULL_POLICY` | `Always` | Pull policy for the images of a build pod, the builder's and the agent's: `Always`, `IfNotPresent` or `Never`; anything else is an error at startup. `Always` picks up a new image; use `IfNotPresent` for images you loaded onto the nodes by hand. |
| `WORKER_KUBE_AGENT_IMAGE` | none, and required; the manifest sets it to the dispatcher's own image | Image build pods take the build agent from. Without it the dispatcher refuses to start. |
| `WORKER_GIT_SSH_KEY`, `WORKER_SSH_KNOWN_HOSTS` | none | The SSH key builds fetch private sources with, and the host keys of the servers it is used with: files in the dispatcher's own pod. Both or neither. See [Build credentials](#build-credentials). |
| `WORKER_KUBE_POD_TEMPLATE` | none | Path of a JSON file adding scheduling fields to every build pod. See [Architectures and scheduling](#architectures-and-scheduling). |

### Settings you can change from AURCache

These are [settings AURCache can set for the worker](./split-chroot.md#setting-values-from-aurcache),
and they apply from the next build. As for the chroot worker, the plain variable
pins the value on the machine, and its `_DEFAULT` form (for example
`WORKER_BUILD_CPUS_DEFAULT`) gives a starting point AURCache may override.

| Variable | Setting | Default | Description |
|---|---|---|---|
| `WORKER_BUILD_MEMORY_MAX` | `build_memory_max` | unset | Memory one build container may use, as its request and its limit. |
| `WORKER_BUILD_CPUS` | `build_cpus` | unset | CPUs one build container may use, as its request and its limit; fractions are allowed. Also sizes the build's parallel jobs. `0` is no limit. |
| `WORKER_BUILD_DISK_MAX` | `build_disk_max` | unset | Disk one build may write. Enforced by eviction; see below. |
| `WORKER_KUBE_SCHEDULE_TIMEOUT` | `schedule_timeout` | `10m` | How long a build pod may wait for a node, then for its first container to start, then for the build container to start once the job is in, before the build is handed back to the server. |
| `WORKER_KEYSERVER` | `keyserver` | the chroot worker's default | Keyserver used to fetch the keys a PKGBUILD names in `validpgpkeys`. Which keys are accepted is decided by the PKGBUILD, not by this. |

Concurrency, priority, package affinity and the build timeout are the same
settings as on every worker. `WORKER_CONCURRENCY` is the most build pods at
once, one total for all architectures.

## Limits are best effort

Limits are applied as well as Kubernetes allows, and a build is never refused
for being imperfectly limited. Treat them as a recommendation that helps the
cluster protect itself, not as a guarantee.

- **Unset means no limit.** All three build limits are unset by default, and a
  value of `0` also means none.
- **A set limit is also the request.** With the request equal to the limit, a
  build gets what it is told it has, and the scheduler places it where that is
  free. A build told it has 16 CPUs therefore waits for a node with 16 free.
- **The disk limit evicts after the fact.** `build_disk_max` becomes the build
  container's `ephemeral-storage` request and limit and the size limit of the
  build's work volume. Kubernetes enforces both by evicting the pod some seconds
  after it passes the limit, so a build is stopped rather than refused at the
  limit, and one that writes quickly can briefly exceed it. The kubelet's own
  eviction thresholds protect the node. The chroot worker's disk limit is
  stricter: its kernel refuses the write.
- **There is no worker-wide total.** A ResourceQuota on the builds' namespace is
  the tool for a ceiling on all builds together. Keep `WORKER_CONCURRENCY`
  within it: a build whose pod the quota refuses is handed back to the server
  as interrupted, and spends one of its attempts. A quota on
  `requests.cpu`, `requests.memory`, `limits.cpu` or `limits.memory` needs the
  per-build limits set, because such a quota makes the cluster refuse any pod
  with a container that does not state the resource. One on
  `requests.ephemeral-storage` refuses no pod, but counts a container that
  does not state it as asking for none, so it needs the disk limit to limit
  anything. A pod count needs nothing. Beside the build
  itself, each build pod asks for 10m of CPU and 16Mi of memory, with limits of
  one CPU and 64Mi, for the container that hands the packages back. Count them
  in.

## Architectures and scheduling

The architectures a dispatcher advertises are configuration: `WORKER_ARCHES`,
or its own architecture when unset. It does not discover them from the nodes,
and it refuses to start with one its image is not built for (anything but
`x86_64`, `aarch64` and `armv7h`): every build pod runs the agent from it.
Each build pod is scheduled with `kubernetes.io/os: linux` and
`kubernetes.io/arch` set for the package's architecture (`x86_64` is `amd64`,
`aarch64` is `arm64`, `armv7h` is `arm`). A build for an
architecture no node has waits for a node until the schedule timeout, then is
handed back to the server, which tries it again while it has attempts left: do
not advertise an architecture the cluster has no node for. Builds under
emulation (binfmt on the nodes) are the operator's business: the worker only
advertises what it is told.

A worker cannot ask the server for a build of one architecture, so while any
pod of a dispatcher's has no node to go to, it claims nothing: the next build
might wait for a node too. Run one dispatcher per architecture: its builds
never wait behind those of an architecture the cluster is short of. Each needs
a builds namespace of its own (see [One replica only](#one-replica-only)).

The dispatcher's image must exist for every architecture you build for, since
part of each pod runs from it. It is built for `x86_64`, `aarch64` and
`armv7h`, and the dispatcher builds for those alone.

For anything more, `WORKER_KUBE_POD_TEMPLATE` names a JSON file, usually
mounted from a ConfigMap, with any of these fields and no others:
`tolerations`, `affinity`, `nodeSelector`, `priorityClassName`,
`runtimeClassName` (for example gVisor or Kata), `imagePullSecrets`,
`schedulerName`, `topologySpreadConstraints`, `labels` and `annotations`. It
cannot add a volume, a container or a security context, and a field outside
that list is an error at startup.

```json
{
  "nodeSelector": {"pool": "builds"},
  "tolerations": [{"key": "builds", "operator": "Exists"}]
}
```

## Build credentials

Packages whose sources are in private git repositories need an SSH key. It comes
from a Secret you create in the **dispatcher's** namespace, mounted into the
dispatcher's pod, and from nowhere else:

```bash
ssh-keyscan github.com > known_hosts
kubectl -n aurcache create secret generic aurcache-build-ssh \
  --from-file=ssh-privatekey=./id_ed25519 \
  --from-file=known_hosts=./known_hosts
```

Mount it into the dispatcher (the commented `build-ssh` volume in
`worker.yaml`), and set `WORKER_GIT_SSH_KEY=/etc/aurcache-ssh/ssh-privatekey`
and `WORKER_SSH_KNOWN_HOSTS=/etc/aurcache-ssh/known_hosts`. The key goes under
`ssh-privatekey`, as in a `kubernetes.io/ssh-auth` Secret. It is any private
key `ssh-add` loads, without a passphrase, since there is no one to enter it.
The dispatcher reads it again at the start of each build, so a changed Secret
is used from the next build on.

It is the dispatcher's default key, and the dispatcher holds no [named
keys](./credentials.md#named-keys), as on the Docker worker.

**The key never enters a build pod.** For each build, the dispatcher loads it
into an OpenSSH `ssh-agent` in its own pod, and ssh in the build pod reaches
that agent over an exec stream while makepkg runs. A build cannot take the
key away, but it can use the key while it runs: to log in as it, or to sign
anything with it. What that means, and which hosts git's ssh trusts, is as on
the [Docker worker](./docker.md#build-credentials): use a key made for builds,
and do not register it as a signing key anywhere. `known_hosts` is required.

The [generated-key onboarding](./credentials.md) of the chroot worker does not
apply here: a key generated in a pod would be lost with it.

## What a build can reach

Every build pod is covered by the NetworkPolicy in `networkpolicy.yaml`:

- nothing can connect to a build;
- a build may resolve names through the cluster's DNS: pods labelled
  `k8s-app: kube-dns`, in any namespace, which is how CoreDNS runs on most
  distributions;
- it may reach the server's repository ports only (80 and 8081 of the server's
  pods), never its worker port;
- it may reach the internet outside private address ranges (`10.0.0.0/8`,
  `172.16.0.0/12`, `192.168.0.0/16`, `100.64.0.0/10`, `169.254.0.0/16`, and for
  IPv6 `fc00::/7` and `fe80::/10`). That keeps builds off other workloads, the
  Kubernetes API and the cloud metadata endpoint, which on many clusters hands
  out the node's credentials.

If your mirror or keyserver is on a private address, add a rule for it to the
policy. If the server is outside the cluster and shares one address and port
between the repository and the worker protocol, no NetworkPolicy can separate
them, and approving a worker remains the trust decision.

The policy does not know your cluster's addresses. Three kinds of cluster need
it edited, and `networkpolicy.yaml` has a comment at each place:

- With **NodeLocal DNSCache** (Kubespray's default, an option on GKE and EKS),
  pods resolve through a link-local address on their node, such as
  `169.254.25.10` or `169.254.20.10`. The policy refuses link-local addresses,
  and the cache is not a `kube-dns` pod, so lookups fail: builds resolve
  nothing, and every one stops at its first `pacman -Sy`. Uncomment the rule
  for it in `networkpolicy.yaml`: an `ipBlock` for that address's `/32` on
  port 53, UDP and TCP. The `nameserver` line of `/etc/resolv.conf` in any pod
  names the address.
- **Pods, nodes or services on public addresses.** "Outside private ranges"
  only keeps builds off a cluster that uses private ones. On an IPv6 cluster
  whose pods have globally routable addresses (EKS in IPv6 mode), the rule for
  the IPv6 internet reaches every pod, the server's worker port included. Add
  the cluster's ranges to the `except` list.
- **An API server on a public endpoint**, as managed control planes often
  have. Builds can reach it. They hold no credential for it, so this is
  exposure and not access; add its address to `except` to close it.

A NetworkPolicy has an effect only where the CNI enforces it, and the API does
not say whether it does. Check that yours does before you rely on it: from a
pod in the builds' namespace labelled
`app.kubernetes.io/managed-by: aurcache-worker-kube`, the server's worker port
and the cloud metadata address (`169.254.169.254`) must not answer, and the
repository builds are given must. The policy does not cover name resolution
through NodeLocal DNSCache, nor a mirror on a private address: either shows up
as failing builds.

A CNI may also put the policy around a pod only some seconds after the pod
starts: kindnet lets a new pod reach everything for about ten seconds. A
build's first seconds are then unprotected. A build has normally run no
package code by then: its pod first waits to be handed the job, and then
AURCache's own agent prepares pacman and upgrades the image before makepkg
starts.

## Not supported

- **Caches.** There are no package or source caches: every build downloads its
  dependencies and fetches its sources, and a VCS package is cloned in full each
  time. A pacman caching mirror inside the cluster, given to builds through the
  mirrorlist, removes the repeated dependency downloads.
- **Persistent build directories.** A pod's volume goes with the pod, so every
  build starts from a fresh tree and the `persistent_builddir` setting has no
  effect on this worker. Nothing is logged about it: the setting is on by
  default, and a fresh tree only costs a full rebuild.
- **Kept failed builds.** The worker has no `keep_failed` setting.
- **Adopting running pods after a restart.** When the dispatcher dies (a crash,
  an out-of-memory kill, a node lost), its next start deletes the pods it left
  before it enrolls, with any other build pod in its builds namespace (one
  left by this worker under an identity it lost, with its data volume), and
  logs `Deleted <n> build pod(s) left by a previous run of this worker`. The
  server ends the builds those pods ran (their log ends with
  `Abandoned: its worker stopped reporting it.`) and queues a new build of each
  package, under a new number, while the package has attempts left. Pods also
  bound themselves: each carries a deadline of its build's timeout plus five
  minutes, so one outlives a dispatcher that never comes back by at most that.
  Five minutes is what the server allows too: that long after a build's
  timeout it takes the build back from a worker that has not reported it
  (`REAP_BACKSTOP_GRACE` on the server), and accepts nothing of it afterwards.
  A build that ends close to its timeout with packages too large to hand back
  in that time is built again; give such a package a longer timeout.
- **`--install` and `--rmdeps` in a package's build flags.** The build's
  system is the pod, which is discarded, so there is nothing to install into
  or to clean up. The flags are left out rather than passed to makepkg, which
  could do neither without root.

## What to expect

Measured on the project's test cluster, a single-node kind cluster on one
machine. A real cluster differs, mostly in how fast its API server relays.

- **Packages travel through the Kubernetes API.** The dispatcher reads each
  package out of the pod over an `exec` stream and uploads it to the server as
  it arrives. A 2 GiB package went from the end of its build to published at
  about 50 MiB/s. Very large packages on a slow or rate-limited API server will
  take correspondingly longer.
- **The dispatcher stays small.** It holds no package in memory or on disk: its
  peak memory during that 2 GiB relay was 12 to 14 MiB, against the manifest's
  256 MiB limit. What it does hold is each running build's source archive (the
  PKGBUILD's directory, as the server sends it), once, from its download until
  the pod has taken it, and for a moment twice as the download completes. Its
  memory limit has to cover about 15 MiB of its own plus
  `WORKER_CONCURRENCY` + 1 times the largest source, and twice
  `WORKER_CONCURRENCY` times at worst, when every build's download completes
  at once; raise it for packages that ship large files beside their PKGBUILD.
- **A cancelled build's pod is gone in 2 to 17 seconds.** The pod is deleted
  with a five-second grace period; the rest is the kubelet.

## Troubleshooting

**Builds stay queued and the worker looks idle.** The dispatcher does not claim
a build while one of its own pods is waiting for a node: the next build might
wait too. Its log says so, once per change:

```
Not claiming builds while pod <name> has no node to run on: <the scheduler's message>
Claiming builds again
```

**A pod stays `Pending`, then the build is ended and tried again.** No node
could take it within `WORKER_KUBE_SCHEDULE_TIMEOUT`. Meanwhile the build's log
says `[worker] no node can take the build pod yet: <the scheduler's message>`,
and the build ends with
`no node could take the build within 10m (WORKER_KUBE_SCHEDULE_TIMEOUT): ...`.
The server queues a new build of the package while it has attempts left. The
scheduler's message gives the reason: the limits ask for more than any node has
free (a set limit is also the request), no node carries the
`kubernetes.io/arch` label for the package's architecture, or the selectors and
tolerations in `WORKER_KUBE_POD_TEMPLATE` match nothing. Lower the limits, add
capacity, or fix the template.

**The dispatcher stops at startup with `the cluster does not admit a build
pod`.** Before it enrolls, it asks the cluster with a server-side dry run
whether it would admit a build pod. The cluster's message names the cause: the
builds' namespace missing or at the wrong Pod Security level, a missing
ServiceAccount, a policy webhook. Check `kubectl -n aurcache-builds get events`.
Right after the startup sweep, pods still being deleted count against a full
`pods` quota, so the dry run can be refused once; the dispatcher then restarts
and passes. A cluster that starts refusing build pods later hands each build
back as interrupted, with the same message.

**A pod is refused with a Pod Security message.** The builds' namespace must be
labelled `pod-security.kubernetes.io/enforce: baseline`. A `restricted` label
there forbids the build container's capabilities and root user, and another
admission policy may forbid the same: allow it for this namespace.

**A pod stays `ContainerCreating` or `Init:0/1`, then the build is ended and
tried again.** A node took the pod and could not start it within
`WORKER_KUBE_SCHEDULE_TIMEOUT`. The build ends with
`the build pod did not start within 10m ...`. The pod's status seldom says
why; `kubectl -n aurcache-builds get events` does. The usual causes are an
image that cannot be pulled and a pod network the node cannot set up.

**Every build fails at `pacman -Sy` with `Could not resolve host`.** Builds
cannot reach the cluster's DNS. On a cluster with NodeLocal DNSCache, see
[What a build can reach](#what-a-build-can-reach).

**`ErrImagePull` or `ImagePullBackOff`.** The nodes cannot pull the builder
image or the dispatcher's image. The node tries again by itself, and the
dispatcher waits for it up to `WORKER_KUBE_SCHEDULE_TIMEOUT`, saying so in the
build's log; then the build ends and the server tries it again while it has
attempts left. A name that cannot be pulled at all (`InvalidImageName`) ends it
at once. Check the registry and the names, add `imagePullSecrets` through the
pod template for a private registry, or load the images onto the nodes and set
`WORKER_KUBE_IMAGE_PULL_POLICY=IfNotPresent`. Every node that
builds pulls the dispatcher's image, not only the one it runs on.

**Builds with an SSH key end at their `credential` step.** The log says
why: `the worker did not open the link` when the dispatcher could not exec into
the build container (check that its Role still allows `pods/exec`). The server
builds the package again. See [Build credentials](#build-credentials).

**The dispatcher will not start.** It names the problem: a pull policy outside
the three, a pod template that cannot be read or has a field outside the allowed
list, an SSH key without its `known_hosts` or one it cannot use, or no
`WORKER_KUBE_AGENT_IMAGE`.

**A build fails with `out of disk: the build passed its ephemeral-storage
limit`.** Its pod was evicted for passing `build_disk_max`
(`WORKER_BUILD_DISK_MAX`). Raise the limit, or unset it. A build in which the
kernel killed a process at the memory limit fails with `out of memory`, naming
`WORKER_BUILD_MEMORY_MAX`; one whose pod was killed whole for it ends without a
report as interrupted, with `OOMKilled` in its reason. Either way, raise
`WORKER_BUILD_MEMORY_MAX`.

