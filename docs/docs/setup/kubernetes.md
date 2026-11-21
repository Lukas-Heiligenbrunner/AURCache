---
sidebar_position: 4
---

# Kubernetes

AURCache runs on Kubernetes as the same two pieces it runs as under Docker:

- the **server** (`aurcache-server`), as a Deployment with one replica and a
  volume for its data;
- **build workers**, which enroll with it, claim builds and upload what they
  build.

The manifests are in
[`kubernetes/`](https://github.com/Lukas-Heiligenbrunner/AURCache/tree/master/kubernetes)
in the repository. They are plain manifests put together with Kustomize, which
is built into `kubectl`. There is no Helm chart. The server is the base
(`kubernetes/server/`, which also applies on its own), and the workers are
chosen in `kubernetes/kustomization.yaml`, one Kustomize component per kind.

:::note
These manifests are newer than the compose files and have had far less use.
If one of them does not work on your cluster, please open an issue and say
which distribution and storage you run.
:::

## Choosing a worker

| Worker | How it builds | Needs |
|---|---|---|
| Chroot workers (`components/chroot-workers`, the default) | Each package in a clean `devtools` chroot, in a StatefulSet of privileged pods | Privileged pods, Linux 6.7+ with btrfs on their nodes, a volume per worker for its storage pool |

The rest of this page is about the chroot workers, apart from the server
itself. Workers can also run outside the cluster and connect to its worker
port (see [Workers outside the cluster](#workers-outside-the-cluster)); the
kinds of worker there are, and what each needs, are listed under [Build
Workers](../workers/index.md).

## Image versions

The manifests name the `git` images, built from the master branch, and pull
them again at every start. That is deliberate and temporary. In a privileged
pod under containerd, `/sys/fs/cgroup` is the node's own cgroup tree rather than
the pod's, and the 0.6.0 worker image's entrypoint hands that directory to the
worker's user: the node's root cgroup ends up owned by it, and stays so after
the pod is gone. The fix is in master, not yet in a release.

Because `git` is pulled at every start, **any restart is an upgrade** until
you pin it: an eviction, a drain or a node reboot brings up master as it is
that day, and the server runs that version's database migrations. Pin what you
run by digest in `kubernetes/kustomization.yaml` (`images:` is there,
commented, for the server and the workers alike). The digest running now is
in the pods' status:

```bash
kubectl -n aurcache get pods -o jsonpath='{range .items[*]}{range .status.containerStatuses[*]}{.image} {.imageID}{"\n"}{end}{end}' | sort -u
```

Once a release after 0.6.0 is out, pin it the same way, with `newTag`. Pick
one at least as new as the `git` image the server last ran, which
`/api/version` names (`0.6.0+g<commit>`): an older release finds migrations in
its database that it does not have, and does not start. Do not move these
manifests to `latest` or `0.6.0` in the meantime.

## What the cluster needs

The server needs nothing unusual: an ordinary pod, which the `baseline` [Pod
Security](https://kubernetes.io/docs/concepts/security/pod-security-admission/)
level admits, and a `ReadWriteOnce` volume. The chroot workers need what they
need under Docker, and some clusters cannot give it.

- **Privileged pods.** Each package is built in a `systemd-nspawn` chroot, and
  the worker formats and mounts its own btrfs storage pool. Both take a
  privileged container, so `components/chroot-workers` raises the `aurcache`
  namespace from `baseline` to `privileged`. A cluster policy (Kyverno,
  Gatekeeper, a managed policy) may still refuse them. Managed modes that never
  run privileged pods, such as GKE Autopilot and EKS on Fargate, cannot run
  them at all. The server can still run there, with workers elsewhere.
- **Linux 6.7 or newer on the workers' nodes, with btrfs.** The worker's
  [storage pool](../workers/storage-pool.md) is a btrfs filesystem with simple
  quotas, which arrived in 6.7. The container brings `btrfs-progs`, but the
  filesystem itself is the node's kernel's, so the node must have the btrfs
  module. Many managed node images ship an older LTS kernel, and some minimal
  container-host systems leave btrfs out. Check on the node:

  ```bash
  uname -r                                        # 6.7 or newer
  grep btrfs /proc/filesystems || modprobe -n btrfs
  ```
- **A kernel that matches cgroups from a pod's own cgroup namespace**: Linux
  6.11 or newer, or a stable kernel with the fix (7f3287db6543, backported to
  6.6.53 among others). The worker holds each build to its [network
  policy](../workers/split-chroot.md#what-builds-can-reach-on-the-network) by
  its cgroup, and a pod, unlike a compose container, cannot be given the host's
  cgroup namespace instead. On an older kernel the worker says so at startup
  and takes no builds, unless `WORKER_BUILD_FIREWALL=0` turns the policy off.

  A worker that cannot open its pool takes no builds and says why in its log.
- **A storage class for raw block volumes**, for the storage pools. Most CSI
  drivers for disks can do this (cloud disks, Ceph RBD, Longhorn, TopoLVM,
  OpenEBS LVM). Where yours cannot, keep the pool in an image file instead
  (see [The storage pool](#the-storage-pool)).
- **cgroup v2 on the workers' nodes** is recommended, and is what current
  distributions use. The worker gives each build a cgroup of its own, which is
  where its memory figure and its [limits](../workers/split-chroot.md#resource-limits)
  come from. Without it builds still run, with no memory figure, and a worker
  configured with limits refuses builds rather than run them unlimited.

## Deploying

From a checkout of the repository:

```bash
# Optional: approve workers automatically (see below)
kubectl apply -f kubernetes/server/namespace.yaml
kubectl -n aurcache create secret generic aurcache-enrollment \
  --from-literal=token="$(openssl rand -hex 32)"

kubectl apply -k kubernetes/
```

This starts the server, nginx for the repository, and one x86_64 chroot worker
in the `aurcache` namespace. `kubectl apply -k kubernetes/server/` starts the
server alone. Keep applying whichever you started with: applying
`kubernetes/server/` over a deployment with chroot workers lowers the namespace
to `baseline` again, and their pods are refused the next time they are created.

To look at it before exposing anything:

```bash
kubectl -n aurcache port-forward svc/aurcache 8080:8080 8081:8081
```

- Web UI / API: `http://localhost:8080`
- Pacman repo: `http://localhost:8081`

The worker appears on the **Workers** page within a minute. With the Secret
above it is approved on its own; without it, it waits as `pending` for you to
approve it there.

Before relying on it, read through `kubernetes/kustomization.yaml` and set:

- `AURCACHE_PUBLIC_URL` in `kubernetes/server/server.yaml`, the address pacman
  clients fetch packages from (see [Exposing the
  server](#exposing-the-server));
- the volume sizes and, if the cluster's default is not the one you want,
  `storageClassName`. Decide them now: a StatefulSet's volume templates cannot
  be changed later without the steps under [Changing the volumes
  later](#changing-the-volumes-later).

## What the manifests set up

### The server

| Resource | Purpose |
|---|---|
| Namespace `aurcache` | Pod Security level `baseline`, or `privileged` with the chroot workers. |
| Deployment `aurcache` | The server and an nginx container that serves the repository. One replica, replaced with `Recreate`. |
| PersistentVolumeClaim `aurcache-data` | `/app`: the database, the repository, the internal worker CA, build logs and the source cache. |
| Service `aurcache` | `8080` web UI and API, `8081` the repository (nginx), `8083` the worker protocol. |

There is one replica because the server is the only writer of the repository
and of a SQLite database, and it keeps the repository's lock in memory. A
second server on the same data would corrupt both. `Recreate` makes an update
stop the old pod before it starts the new one, so the server is down for the
moment an update takes. A build whose lease runs out meanwhile (a minute
without a heartbeat, by default) may be requeued, so keep that moment short.

Everything the server keeps lives under `/app`, as in the [compose
setup](./docker.md#what-to-persist). Losing that volume loses the worker CA,
and with it every enrolled worker, so it is the one to back up.

nginx runs in the server's pod rather than in a Deployment of its own because
the data volume is `ReadWriteOnce`: only one node can mount it. It reads the
volume read-only and the server stays its only writer. It runs with a
read-only root filesystem and four capabilities: `NET_BIND_SERVICE` to bind
port 80, `CHOWN` to give its cache directories to its own user, and `SETUID`
and `SETGID` to switch its workers to that user.

Some provisioners, `local-path` among them, create a volume's directory
world-writable. The server closes its own state directories under `/app` at
every start, and an init container closes `/app` itself (mode `0755`), so no
other user in the pod can create files beside them.

The server runs as root, as it does under Docker: each PKGBUILD parse runs as a
separate unprivileged user, and switching to it takes the default `SETUID` and
`SETGID` capabilities. See [Package
parsing](../Configuration/package-parsing.md).

The pod is ready once the server answers HTTP (`/api/version`). It
deliberately does not use `/api/health`, which also pings the database: with
PostgreSQL, a moment without the database would take the whole pod out of the
Service, and the repository and the worker port with it. Point your monitoring
at `/api/health` instead.

### The chroot workers

| Resource | Purpose |
|---|---|
| StatefulSet `aurcache-worker` | x86_64 workers, on `kubernetes.io/arch: amd64` nodes. One replica to start with. |
| Volume `data` (per replica) | `/var/lib/aurcache-worker`: the worker's identity, its SSH key, and the pool's mount point. 1 GiB. |
| Volume `pool` (per replica) | The storage pool, a raw block volume attached at `/dev/aurcache-pool`. 200 GiB. |
| Service `aurcache-workers` | Headless, only because a StatefulSet names one. Workers serve nothing. |

A worker is known to the server by its key, which it keeps on its `data`
volume. A StatefulSet keeps each replica's volumes through restarts, so a
replica stays the same approved worker. It can move to another node with them
when the storage is network-attached (cloud disks, Ceph, Longhorn). On
node-local storage (TopoLVM, OpenEBS LVM, `local-path`), a replica is tied to
its node: while that node is gone, the pod stays `Pending`.

Scaling up adds workers, each of which enrolls as a new one:

```bash
kubectl -n aurcache scale statefulset aurcache-worker --replicas=3
```

Replicas are spread over nodes where the cluster allows it. Two on one node
work, but compete for its CPU, memory and disk. Each build already uses every
core, so before adding replicas on one node, consider raising the worker's
concurrency on its page in the web UI instead.

Each pod also has:

- `privileged: true` and a memory-backed `emptyDir` at `/run`, the same
  configuration as the compose files' `privileged` and tmpfs `/run`;
- an init container that runs the worker package's `systemd-tmpfiles`
  declaration as root. The worker runs as its own `aurcache` user and runs
  each build as `builder`, so its data directory needs the owners and modes
  that declaration gives. Docker fills a new volume from the image with them; a
  new PersistentVolume is empty and owned by root;
- `automountServiceAccountToken: false`, as the server has: neither talks to
  the Kubernetes API, so neither carries a token for it;
- `enableServiceLinks: false`, as the server has. Otherwise Kubernetes sets a
  `<SERVICE>_PORT` variable for every Service in the namespace. The server
  reads `AURCACHE_HTTP_PORT`, `AURCACHE_REPO_PORT` and `AURCACHE_WORKER_PORT`
  as port numbers and stops on anything else, so a Service named
  `aurcache-http`, `aurcache-repo` or `aurcache-worker` in the namespace would
  stop it.

A pod's `resources` limits bound the worker and every build it runs. Within
them, the worker's [own limits](../workers/split-chroot.md#resource-limits)
decide which build gives way first. Keep `WORKER_TOTAL_BUILD_MEMORY_MAX` under
the pod's memory limit, so that a build over it fails with a reason rather than
the whole pod being killed.

A stopped worker's builds are requeued once their lease runs out, a minute by
default, and start again on another worker. To drain a node without losing
work, [stop the worker's intake](../workers/managing.md#stopping-a-workers-intake)
first, and resume it once the node is back. A stopped intake survives the
worker restarting, so without the resume it never builds again.
`--delete-emptydir-data` is needed because the worker's `/run` and nginx's
cache are `emptyDir` volumes, which `kubectl drain` otherwise refuses to
evict; nothing in them outlives a restart anyway:

```bash
aurcli worker pause <id> --wait && kubectl drain <node> --ignore-daemonsets --delete-emptydir-data
# ... maintenance, then:
kubectl uncordon <node>
aurcli worker resume <id>
```

## Approving workers

A new worker is `pending` until it is approved. The manifests offer two ways
past that:

- **A shared token.** Server and workers read `AURCACHE_ENROLLMENT_TOKEN` from
  the `aurcache-enrollment` Secret, and the server approves a worker that
  presents the same token. The Secret is optional in both: without it, no
  token is set on either side. Both read it at start, so after creating or
  changing it, restart them:

  ```bash
  kubectl -n aurcache rollout restart deployment/aurcache statefulset/aurcache-worker
  # and, with the arm64 workers:
  kubectl -n aurcache rollout restart statefulset/aurcache-worker-arm64
  ```

- **The Workers page.** Without the Secret, approve each worker there once.
  Its approval is kept with its key, so restarts and moves do not ask again.

`AURCACHE_PREAPPROVED_WORKERS`, which approves exactly the fingerprints it
lists, is commented in the server's manifest. It suits a fixed set of workers
better than a StatefulSet, whose new replicas each have a new fingerprint.

The shared enrollment volume the bundled compose file uses is not set up: it
takes a volume the server and every worker can mount at once
(`ReadWriteMany`), which most clusters' default storage cannot provide. None of
these paths ever re-approves a worker you revoked; see
[Approving](../workers/managing.md#approving).

## The storage pool

By default each worker's pool is a raw block volume, which the
[storage pool](../workers/storage-pool.md#a-block-device-a-zvol-or-a-partition)
page calls a block device. The worker formats it with btrfs the first time and
mounts it itself, with no loop device and no filesystem nested in another. A
device that already holds a filesystem the worker did not create is refused,
never formatted, so a new volume is what it wants.

`WORKER_DISK_MAX_DEFAULT` is set to `190G` for a 200 GiB volume: the worker
makes its filesystem 5% larger than `WORKER_DISK_MAX` (at least 2 GiB larger),
so that has to fit on the volume.

**Without raw block volumes**, uncomment `components/pool-image` in
`kubernetes/kustomization.yaml`, after the chroot worker components. The pool
is then a sparse image file on the worker's `data` volume, which the component
makes 210 GiB, and is loop-mounted. It works on any storage class, at the cost
of the loop device and the nested filesystem.

:::warning A storage class that does not enforce sizes
`local-path` and other host-path provisioners ignore the size a claim asks for:
the image lives on the node's own disk and can grow to fill it. The worker's
`WORKER_DISK_MAX` is then the only bound, so set it within what that disk has
free, leaving room for the node itself. A node whose disk fills up starts
evicting pods, the server's included.
:::

### Changing the volumes later

A StatefulSet's `volumeClaimTemplates` cannot be changed once it exists:
`kubectl apply` refuses a new pool size, or `components/pool-image` added to a
running deployment. Recent Kubernetes says `spec.volumeClaimTemplates: Invalid
value: ...: field is immutable`; older releases say `spec: Forbidden: updates
to statefulset spec for fields other than ... are forbidden`. The claims it already made are ordinary PersistentVolumeClaims,
though, and are changed one by one.

**To grow a pool**, if the storage class allows volume expansion, expand each
replica's claim, then raise `WORKER_DISK_MAX` on the worker's page (see
[Resizing](../workers/storage-pool.md#resizing)):

```bash
kubectl -n aurcache patch pvc pool-aurcache-worker-0 \
  -p '{"spec":{"resources":{"requests":{"storage":"300Gi"}}}}'
```

With `components/pool-image`, the claim to grow is `data-aurcache-worker-0`.
For replicas added later to get the new size too, change it in the manifest and
replace the StatefulSet without touching its pods or claims:

```bash
kubectl -n aurcache delete statefulset aurcache-worker --cascade=orphan
kubectl apply -k kubernetes/
```

**To switch between a block volume and an image file**, each worker's pool
starts again, empty: it only ever holds the base chroot and caches, which the
worker rebuilds. Its identity, on `data`, can stay.

1. Stop each worker's intake and wait for its builds:
   `aurcli worker pause <id> --wait`.
2. Replace the StatefulSet, leaving its pods running:
   `kubectl -n aurcache delete statefulset aurcache-worker --cascade=orphan`.
3. Change the claims. Moving to an image file, the `data` claims must grow to
   hold it: expand each `data-aurcache-worker-N` as above (if the class cannot
   expand, delete it, and that replica enrolls again as a new worker), and
   delete each `pool-aurcache-worker-N`. Moving to a block volume, delete
   nothing: the new template makes the `pool` claims.
4. Change `kubernetes/kustomization.yaml`, `kubectl apply -k kubernetes/`, and
   delete the old pods so they are made again from the new template:
   `kubectl -n aurcache delete pod -l app.kubernetes.io/instance=aurcache-worker`.
5. Moving back to a block volume, delete the old image once the worker is up
   again, or it keeps its space on the `data` volume:
   `kubectl -n aurcache exec aurcache-worker-0 -- sudo rm /var/lib/aurcache-worker/chroot/pool.img`.
6. Resume each worker's intake: `aurcli worker resume <id>`.

With `components/chroot-workers-arm64`, do the same for `aurcache-worker-arm64`:
its claims end in `-aurcache-worker-arm64-N`, and its pods carry
`app.kubernetes.io/instance=aurcache-worker-arm64`.

## What builds can reach on the network

A chroot build runs in its worker pod's network namespace, so package code can
connect wherever the worker can. The worker puts
[a network policy](../workers/split-chroot.md#what-builds-can-reach-on-the-network)
on each build: nothing on a local address -- private, link-local, carrier-grade
NAT, or on the pod's own links -- unless allowed (the repository is), so the
server's other ports are refused, and the internet open. It matches the
build's cgroup, so it holds root's connections in the build's container as
well as the build user's, and leaves the worker process alone. Without it, or
for what it leaves open, a PKGBUILD could reach:

- **the server's web API**, `aurcache:8080`. Without
  [OAuth](../Configuration/authentication.md) configured, anyone who reaches it
  can use it without logging in: list, add and delete packages, change
  settings;
- other Services in the cluster, the Kubernetes API and the nodes, including
  those on public addresses, which the worker's policy allows;
- the cloud metadata endpoint (`169.254.169.254`), which on some clouds hands
  out the node's credentials.

Configure OAuth, enable the NetworkPolicy below, or both. The NetworkPolicy is
enforced outside the pod, on the worker process as much as on its builds, and
keeps both off port 8080 whether or not OAuth is on.

`components/chroot-worker-egress` is a NetworkPolicy for the worker pods that
allows DNS, the server's repository and worker ports, and public addresses, and
refuses private, link-local, carrier-grade NAT, class E and NAT64 ranges. It applies to the
worker process as much as to its builds, since they share an address. Before
enabling it:

- your CNI must enforce NetworkPolicy (Calico, Cilium, kube-router, k3s's
  built-in one; not flannel alone), or the policy does nothing;
- its DNS rule allows pods labelled `k8s-app: kube-dns`, in any namespace,
  which is how CoreDNS runs on most distributions. If yours differs, every
  name lookup fails until you adjust it;
- with **NodeLocal DNSCache** (Kubespray's default, an option on GKE and EKS),
  pods resolve through a cache on their own node, which is not a `kube-dns`
  pod. It listens on a link-local address such as `169.254.25.10` or
  `169.254.20.10` and, in some setups (GKE's, and iptables mode), on the
  kube-dns Service's ClusterIP too, which the node intercepts. The policy
  refuses both, so lookups fail. Uncomment the rule for it in the component's
  `networkpolicy.yaml`, with an `ipBlock` `/32` on port 53, UDP and TCP, for
  each address the cache listens on. A pod's `/etc/resolv.conf` may name only
  the ClusterIP; the cache's own arguments list them all:

  ```bash
  kubectl -n kube-system get daemonset node-local-dns \
    -o jsonpath='{.spec.template.spec.containers[0].args}'   # see -localip
  ```

- anything builds must reach on a private address (a source mirror, a git
  server, an HTTP proxy) needs a rule of its own.

"Public addresses" means every address outside those private ranges,
including whatever of the cluster's own has one: a managed control plane's
API endpoint, nodes with public IPs, and the kubelet and NodePorts on them.
The policy keeps builds off the API server and the nodes only where those are
on private addresses. Where they are not, add their ranges to the `except`
list of the public-address rule (an example is commented there). The same goes
for a cluster whose pods or nodes have global IPv6 addresses, such as EKS in
IPv6 mode: the policy cannot know those ranges, so add them too.

Where node credentials matter, also block the metadata endpoint for pods at the
cloud's own level (IMDSv2 with a hop limit of 1 on AWS, Workload Identity on
GKE), whatever this policy does.

## Other architectures

A chroot worker builds for the architecture of the node it runs on, so each
architecture is a StatefulSet of its own, pinned to nodes of that architecture.
`components/chroot-workers-arm64` adds `aurcache-worker-arm64`, the x86_64 one
renamed, with its own labels and volumes, on `kubernetes.io/arch: arm64` nodes,
building aarch64 packages. It works with or without the x86_64 workers. The
worker image is multi-arch, so those nodes pull their own variant of the same
tag. For armv7h, copy `kubernetes/chroot-worker-arm64/` and its component with
`arm` and `armv7h` in place of `arm64` and `aarch64`.

Emulated workers, which the compose file runs on x86_64 through qemu, are not
set up here: they need binfmt handlers registered on the node, which is the
node's configuration rather than the cluster's.

## Exposing the server

The `aurcache` Service is `ClusterIP`. To reach the web UI and the repository
from outside the cluster, route an Ingress (or a `LoadBalancer` Service) to it:

```yaml
apiVersion: networking.k8s.io/v1
kind: Ingress
metadata:
  name: aurcache
  namespace: aurcache
spec:
  # ingressClassName: <yours>
  rules:
    - host: aurcache.example.com
      http:
        paths:
          - path: /
            pathType: Prefix
            backend:
              service:
                name: aurcache
                port:
                  name: http
    - host: repo.aurcache.example.com
      http:
        paths:
          - path: /
            pathType: Prefix
            backend:
              service:
                name: aurcache
                port:
                  name: repo
```

Then set `AURCACHE_PUBLIC_URL` to the repository's address
(`https://repo.aurcache.example.com` here). The repository is served from the
root of its host, so a host of its own is simpler than a path under the UI's.

Put authentication in front of the UI, or configure
[OAuth](../Configuration/authentication.md), before exposing it beyond a
network you trust.

### Workers outside the cluster

The worker port, `8083`, is mutual TLS: each worker authenticates with its
certificate. An Ingress that terminates TLS cannot carry that, so expose the
port with a `LoadBalancer` or `NodePort` Service, or with TLS passthrough:

```yaml
apiVersion: v1
kind: Service
metadata:
  name: aurcache-worker-api
  namespace: aurcache
spec:
  type: LoadBalancer
  selector:
    app.kubernetes.io/name: aurcache
    app.kubernetes.io/component: server
  ports:
    - name: worker
      port: 8083
      targetPort: worker
```

Add the hostname workers dial to `AURCACHE_TLS_SANS` in
`kubernetes/server/server.yaml`. The server issues its certificate again at
every start, so the change takes a restart. A worker on another machine is
then set up like any [remote
worker](https://github.com/Lukas-Heiligenbrunner/AURCache/blob/master/compose/docker-compose.remote-worker.yaml),
with `AURCACHE_URL=https://<that hostname>:8083`, and should pin the server's
CA with `AURCACHE_SERVER_CA_FINGERPRINT`.

## PostgreSQL

SQLite on the server's volume is the default. To use a PostgreSQL you already
run (an operator such as CloudNativePG, or a managed database), uncomment
`components/postgres` in `kubernetes/kustomization.yaml` and give the server its
address and credentials:

```bash
kubectl -n aurcache create secret generic aurcache-postgres \
  --from-literal=host=<host>[:<port>] \
  --from-literal=user=<user> \
  --from-literal=password=<password> \
  --from-literal=database=<database>
```

Any password works: the server escapes the user and password itself when it
builds its connection URL. The data volume is still needed: the repository,
the worker CA and the build logs stay on it.

## Upgrading

The `git` images are pulled again whenever a pod starts. Restart the server
first, then the workers:

```bash
kubectl -n aurcache rollout restart deployment/aurcache
kubectl -n aurcache rollout status deployment/aurcache
kubectl -n aurcache rollout restart statefulset/aurcache-worker
kubectl -n aurcache rollout restart statefulset/aurcache-worker-arm64   # if you run it
```

With a version pinned (see [Image versions](#image-versions)), change it in
`kubernetes/kustomization.yaml` and apply instead.

## Removing it

`kubectl delete -k kubernetes/` deletes everything it applied, the `aurcache`
namespace included, and deleting a namespace deletes every
PersistentVolumeClaim in it. With the reclaim policy most default storage
classes use (`Delete`), that deletes the server's volume -- the database, the
repository and the worker CA -- along with the workers' volumes and the
Secrets you created by hand.

To keep the data, set the server's volume to be retained before deleting:

```bash
kubectl patch pv "$(kubectl -n aurcache get pvc aurcache-data -o jsonpath='{.spec.volumeName}')" \
  -p '{"spec":{"persistentVolumeReclaimPolicy":"Retain"}}'
```

A retained volume outlives its claim, as `Released`, and keeps a reference to
it. To use it again, for a new deployment, clear that reference so it becomes
`Available`, then let a claim of the same name and size bind it (pinning
`volumeName` in the claim makes sure it is that one):

```bash
kubectl patch pv <name> --type json -p '[{"op":"remove","path":"/spec/claimRef"}]'
```

Or remove only the workloads, and leave the namespace and its claims:

```bash
kubectl -n aurcache delete deployment/aurcache statefulset/aurcache-worker
kubectl -n aurcache delete statefulset/aurcache-worker-arm64   # if you run it
```

## Docker-based builders on Kubernetes

The container builder runs each build in a container it creates through a
Docker API, on a Unix socket. A Kubernetes node's runtime is containerd or
CRI-O, which offer none, so it is not part of these manifests.

The [hybrid image](../workers/hybrid.md) in its DinD mode (`BUILD_ARTIFACT_DIR`
unset) needs no outside daemon: its builds run in a Podman inside its own
container. That container needs either `privileged: true` or `SYS_ADMIN` with
the seccomp and AppArmor settings described under [Without
`privileged`](../workers/hybrid.md#without-privileged). On Kubernetes both put
the namespace at the `privileged` Pod Security level, the second because
`baseline` refuses `SYS_ADMIN`, and the seccomp profile has to be installed on
each node as a `Localhost` profile.

Against a separate Docker daemon, the only shape that works is a privileged
`docker:dind` container in the same pod, sharing the socket through an
`emptyDir`, with the build directory mounted **at the same path** in both
containers. The builder tells the daemon to bind `BUILD_ARTIFACT_DIR` into each
build container, and reads the results from the same directory itself, so the
path has to mean the same directory to both.

## Troubleshooting

**A worker's PVC stays `Pending`.** Its storage class cannot provision the
volume: often `volumeMode: Block`, which `local-path` and other host-path
provisioners do not support. `kubectl -n aurcache describe pvc
pool-aurcache-worker-0` says so in its events (`rancher.io/local-path does not
support block volume provisioning`, for one). Use `components/pool-image`, or
name a class that can in the `pool` claim template.

**`kubectl apply` says `spec.volumeClaimTemplates` is immutable** (or, on
older releases, that updates to the StatefulSet spec are forbidden). A volume
template changed: a size, a storage class, or `components/pool-image`.
See [Changing the volumes later](#changing-the-volumes-later).

**`components/pool-image` changed nothing: the workers still have a `pool`
claim.** A component only changes what the components before it have added,
so it has to come after `components/chroot-workers` (and the arm64 one) in
`kubernetes/kustomization.yaml`. Listed before them, it finds no StatefulSet
and does nothing, without an error.

**The worker logs that it cannot open its storage pool.** The node's kernel is
older than 6.7, or has no btrfs. Run the worker on nodes that have both (a
`nodeSelector` or node affinity on the StatefulSet).

**The worker cannot connect: the certificate is not valid for the name.** The
host in the worker's `AURCACHE_URL` is not in the server's `AURCACHE_TLS_SANS`.
The manifests list `aurcache` and its namespaced forms in the `aurcache`
namespace; deploying to another namespace means changing those too, with
everything else `kubernetes/server/kustomization.yaml` lists.

**`Failed to create the directory $PKGDEST`, with the node's Docker or Podman
socket mounted into the pod.** The daemon binds `BUILD_ARTIFACT_DIR` from the
node's filesystem, and a volume mounted in the pod is not on the node at that
path, so each build container gets an empty or missing directory. Use the chroot
workers, or the same-path `docker:dind` shape above
([#200](https://github.com/Lukas-Heiligenbrunner/AURCache/issues/200)).
