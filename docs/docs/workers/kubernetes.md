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
  host network or host PID. Only [user namespaces](#user-namespaces), which are
  optional, ask anything of the nodes.
- **A namespace for the builds** (`aurcache-builds` by default) at the Pod
  Security `baseline` level. The dispatcher's own namespace needs nothing of the
  sort: the dispatcher passes `restricted`.
- **A CNI that enforces NetworkPolicy** (Calico, Cilium, kube-router, kindnet;
  not flannel alone). The manifests firewall build pods with a NetworkPolicy,
  and without enforcement that policy does nothing. See
  [What a build can reach](#what-a-build-can-reach).
- **RBAC**, namespace-scoped. In the builds' namespace the dispatcher may
  create, get, list, watch and delete pods (and delete them by label), open
  `exec` sessions into them and read their logs. In its own namespace it may
  read pods, to learn which image it runs. It holds no cluster-wide role.
- **A small volume** for the dispatcher's identity: a 1 Gi `ReadWriteOnce`
  claim from the default StorageClass, or one you name. Packages stream through
  the dispatcher and are never stored.
- **Nodes labelled `kubernetes.io/arch`** (every node is) for each architecture
  you build for.
- **Image pulls**: the builder image (about 1 GB compressed) once per node, and
  the dispatcher's image on every node that builds, because each build pod
  starts from it to install the build agent.

## Installing

Apply the server's manifests first, then the worker's, from the repository
root:

```bash
kubectl apply -k kubernetes/
kubectl apply -k kubernetes/kube-worker/
```

The worker's kustomization is separate from the server's because its resources
live in two namespaces and name them. It expects the server's Service
`aurcache` in the namespace `aurcache`, and creates:

| Resource | Where | Purpose |
|---|---|---|
| Namespace `aurcache-builds`, labelled for Pod Security `baseline` | cluster | where build pods run |
| ServiceAccount, Roles and RoleBindings for the dispatcher | `aurcache` and `aurcache-builds` | the permissions above |
| ServiceAccount `aurcache-build` | `aurcache-builds` | what build pods run as; bound to nothing, with no API token mounted |
| NetworkPolicy | `aurcache-builds` | the firewall around build pods |
| Deployment `aurcache-kube-worker` and its volume | `aurcache` | the dispatcher: one replica, strategy `Recreate`, unprivileged, a read-only root filesystem |
| ResourceQuota | `aurcache-builds` | an optional ceiling on all builds together; not applied unless enabled in `kustomization.yaml` |

Render the result without a cluster with
`kubectl kustomize kubernetes/kube-worker/`.

The dispatcher's image is also the image of the agent that runs in every build
pod, so one tag moves both. Pin a release instead of following `latest` with
`kustomize edit set image`; the comments in `kustomization.yaml` show how.

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
credential), copy `worker.yaml` and the ServiceAccount, Roles and RoleBindings
under other names: it enrolls as a worker of its own and can share the builds'
namespace.

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
| `WORKER_KUBE_IMAGE_PULL_POLICY` | `Always` | Pull policy for the builder image: `Always`, `IfNotPresent` or `Never`; anything else is an error at startup. `Always` picks up a new image; use `IfNotPresent` for an image you loaded onto the nodes by hand. |
| `WORKER_KUBE_AGENT_IMAGE` | the image the dispatcher's own pod runs | Image build pods take the build agent from. |
| `WORKER_KUBE_POD_NAME`, `WORKER_KUBE_POD_NAMESPACE` | none; the manifest sets them from the downward API | Which pod to read the agent image from. Needed unless `WORKER_KUBE_AGENT_IMAGE` is set; with neither, the dispatcher refuses to start. |
| `WORKER_KUBE_CREDENTIAL_SECRET` | none | A Secret in the builds' namespace holding an SSH build credential. See [Build credentials](#build-credentials). |
| `WORKER_KUBE_USER_NAMESPACES` | `false` | `true` or `false`. Run build pods in user namespaces. See [User namespaces](#user-namespaces). |
| `WORKER_KUBE_POD_TEMPLATE` | none | Path of a JSON file adding scheduling fields to every build pod. See [Architectures and scheduling](#architectures-and-scheduling). |
| `WORKER_KUBE_NETWORK_PROBE` | `true` | `true` or `false`. Check at startup that the NetworkPolicy is enforced. |

The two yes/no variables accept `true` or `false` in any case; anything else is
an error at startup.

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
| `WORKER_KUBE_SCHEDULE_TIMEOUT` | `schedule_timeout` | `10m` | How long a build pod may stay unschedulable before the build is given up on. |
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
  the tool for a ceiling on all builds together, and the dispatcher honours it:
  it does not claim a build the namespace has no room for. A quota on
  `requests.cpu` or `requests.memory` needs the per-build limits set, and one on
  `requests.ephemeral-storage` needs the disk limit, because a quota makes the
  cluster refuse any pod that does not state the resource. A pod count needs
  nothing.

## Architectures and scheduling

The architectures a dispatcher advertises are configuration: `WORKER_ARCHES`,
or its own architecture when unset. It does not discover them from the nodes.
Each build pod is scheduled with `kubernetes.io/os: linux` and
`kubernetes.io/arch` set for the package's architecture (`x86_64` is `amd64`,
`aarch64` is `arm64`, `armv7h` is `arm`, `riscv64` is `riscv64`). A build for an
architecture no node has stays unschedulable until the schedule timeout, then is
requeued. Builds under emulation (binfmt on the nodes) are the operator's
business: the worker only advertises what it is told.

The dispatcher's image must exist for every architecture you build for, since
part of each pod runs from it.

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

## User namespaces

With `WORKER_KUBE_USER_NAMESPACES=true` every build pod runs with
`hostUsers: false`: root in the build is an unprivileged user range on the
node, so a hostile dependency's install script and a container escape both land
as no one in particular. It is off by default so that the worker runs on any
cluster, and worth turning on wherever the cluster supports it. It is read from
the dispatcher's environment only, so the server cannot turn it off.

It needs, on the nodes:

- Linux 6.3 or later, and idmapped mounts on the filesystems under
  `/var/lib/kubelet/pods/` and in every volume (btrfs, ext4, xfs and tmpfs
  work; NFS does not);
- containerd 2.0 or later, or CRI-O 1.25 or later;
- runc 1.2 or later, or crun 1.9 or later;
- a Kubernetes version with user namespace support: the feature appeared in
  1.28, and the manifests' comments ask for 1.33 or newer.

## Build credentials

Packages whose sources are in private git repositories need an SSH key. It comes
from a Secret you create in the builds' namespace, and from nowhere else:

```bash
kubectl -n aurcache-builds create secret generic aurcache-build-ssh \
  --from-file=ssh-privatekey=./id_ed25519 \
  --from-file=known_hosts=./known_hosts
```

Set `WORKER_KUBE_CREDENTIAL_SECRET=aurcache-build-ssh`. The key goes under
`ssh-privatekey`, as in a `kubernetes.io/ssh-auth` Secret, and `known_hosts` is
optional. The Secret is mounted readable by root alone, in the build container
only. A build can use the key through an `ssh-agent` for its duration but cannot
read it. One key is given to every build the dispatcher runs; use
[routing](./routing.md) to send the packages that need it to this worker.

The [generated-key onboarding](./credentials.md) of the chroot worker does not
apply here: a key generated in a pod would be lost with it.

## What a build can reach

Every build pod is covered by the NetworkPolicy in `networkpolicy.yaml`:

- nothing can connect to a build;
- a build may resolve names through the cluster's DNS;
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

A NetworkPolicy has an effect only where the CNI enforces it, and the API does
not say whether it does. At startup the dispatcher therefore runs a short pod
that tries to reach the server's worker port and the metadata address, and logs
an error if either answers. Set `WORKER_KUBE_NETWORK_PROBE=false` to skip it,
for example where you have confirmed enforcement or accepted the exposure.

## Not supported

- **Caches.** There are no package or source caches: every build downloads its
  dependencies and fetches its sources, and a VCS package is cloned in full each
  time. A pacman caching mirror inside the cluster, given to builds through the
  mirrorlist, removes the repeated dependency downloads.
- **`persistent_builddir`.** A pod's volume goes with the pod. A package that
  asks for it is built in a fresh tree, and its build log and the worker's log
  say so.
- **Kept failed builds.** The worker has no `keep_failed` setting.
- **Adopting running pods after a restart.** When the dispatcher restarts, the
  server abandons the builds it held, and the dispatcher deletes the pods those
  builds ran in before it enrolls. Their progress is lost and the builds are
  requeued, as on any worker. Pods also bound themselves: each carries a
  deadline of the build's timeout plus ten minutes, so one outlives a dead
  dispatcher by at most that.

## Troubleshooting

**Builds stay queued and the worker looks idle.** The dispatcher does not claim
a build while the cluster could not start another pod: while one of its pods is
unschedulable, and while a server-side dry run of a build pod is refused. When
the dry run is refused, the dispatcher logs the cluster's message, once per
change. It names the cause: a ResourceQuota with no room, the builds' namespace
at the wrong Pod Security level, a missing ServiceAccount, or a policy webhook.
Check `kubectl -n aurcache-builds get events` and
`kubectl -n aurcache-builds describe resourcequota`.

**A pod stays `Pending`, then the build is requeued.** No node could take it
within `WORKER_KUBE_SCHEDULE_TIMEOUT`. `kubectl -n aurcache-builds describe pod <pod>`
shows the scheduler's reason: the limits ask for more than any node has free
(a set limit is also the request), no node carries the `kubernetes.io/arch`
label for the package's architecture, or the selectors and tolerations in
`WORKER_KUBE_POD_TEMPLATE` match nothing. Lower the limits, add capacity, or fix
the template.

**A pod is refused with a Pod Security message.** The builds' namespace must be
labelled `pod-security.kubernetes.io/enforce: baseline`. A `restricted` label
there forbids the build container's capabilities and root user, and another
admission policy may forbid the same: allow it for this namespace.

**`ErrImagePull` or `ImagePullBackOff`.** The nodes cannot pull the builder
image or the dispatcher's image. Check the registry and the names, add
`imagePullSecrets` through the pod template for a private registry, or load the
images onto the nodes and set `WORKER_KUBE_IMAGE_PULL_POLICY=IfNotPresent`.
Every node that builds pulls the dispatcher's image, not only the one it runs
on.

**The dispatcher will not start.** It names the problem: a yes/no variable that
is not `true` or `false`, a pull policy outside the three, a pod template that
cannot be read or has a field outside the allowed list, or no agent image (set
`WORKER_KUBE_AGENT_IMAGE`, or both `WORKER_KUBE_POD_NAME` and
`WORKER_KUBE_POD_NAMESPACE`).

**A build ends with its pod evicted.** It passed `build_disk_max`. Raise the
limit, or unset it.

**The startup log reports that the NetworkPolicy is not enforced.** Builds can
reach whatever any pod in the cluster can. Enable NetworkPolicy support in your
CNI, or move to one that enforces it.
