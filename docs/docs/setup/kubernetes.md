---
sidebar_position: 4
---

# Kubernetes

AURCache runs on Kubernetes as the same two pieces it runs as under Docker:

- the **server** (`aurcache-server`), as a Deployment with one replica and a
  volume for its data;
- **build workers** (`aurcache-worker`), as a StatefulSet. Each replica is one
  worker with volumes of its own, and builds every package in a clean
  `devtools` chroot.

The manifests are in
[`kubernetes/`](https://github.com/Lukas-Heiligenbrunner/AURCache/tree/master/kubernetes)
in the repository. They are plain manifests put together with Kustomize, which
is built into `kubectl`. There is no Helm chart.

:::note
These manifests are newer than the compose files and have had far less use.
If one of them does not work on your cluster, please open an issue and say
which distribution and storage you run.
:::

## What the cluster needs

The server needs nothing unusual: an ordinary pod and a `ReadWriteOnce` volume.
The worker needs the same things it needs under Docker, and some clusters
cannot give them.

- **Privileged pods.** Each package is built in a `systemd-nspawn` chroot, and
  the worker formats and mounts its own btrfs storage pool. Both take a
  privileged container. The manifests' namespace is labelled for the
  `privileged` [Pod Security
  Admission](https://kubernetes.io/docs/concepts/security/pod-security-admission/)
  level, which a cluster policy (Kyverno, Gatekeeper, a managed policy) may
  still refuse. Managed modes that never run privileged pods, such as GKE
  Autopilot and EKS on Fargate, cannot run the worker. The server can still
  run there, with workers on other machines (see [Workers outside the
  cluster](#workers-outside-the-cluster)).
- **Linux 6.7 or newer on the worker's nodes, with btrfs.** The worker's
  [storage pool](../workers/storage-pool.md) is a btrfs filesystem with simple
  quotas, which arrived in 6.7. The container brings `btrfs-progs`, but the
  filesystem itself is the node's kernel's, so the node must have the btrfs
  module. Many managed node images ship an older LTS kernel, and some minimal
  container-host systems leave btrfs out. Check on the node:

  ```bash
  uname -r                                        # 6.7 or newer
  grep btrfs /proc/filesystems || modprobe -n btrfs
  ```

  A worker that cannot open its pool takes no builds and says why in its log.
- **cgroup v2 on the worker's nodes**, which current distributions use by
  default. The worker gives each build a cgroup of its own for its memory
  figure and its limits.
- **A storage class for raw block volumes**, for the storage pool. Most CSI
  drivers for disks can do this (cloud disks, Ceph RBD, Longhorn, TopoLVM,
  OpenEBS LVM). Where yours cannot, keep the pool in an image file instead
  (see [The storage pool](#the-storage-pool)).

## Deploying

From a checkout of the repository:

```bash
# Optional: approve workers automatically (see below)
kubectl create namespace aurcache
kubectl -n aurcache create secret generic aurcache-enrollment \
  --from-literal=token="$(openssl rand -hex 32)"

kubectl apply -k kubernetes/
```

This starts the server, nginx for the repository, and one x86_64 worker in the
`aurcache` namespace. To look at it before exposing anything:

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
  `storageClassName`;
- an image version instead of `latest`, if you would rather upgrade on purpose
  (see [Upgrading](#upgrading)).

## What the manifests set up

### The server

| Resource | Purpose |
|---|---|
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
volume read-only and the server stays its only writer.

The server runs as root, as it does under Docker: each PKGBUILD parse runs as a
separate unprivileged user, and switching to it takes the default `SETUID` and
`SETGID` capabilities. See [Package
parsing](../Configuration/package-parsing.md).

### The workers

| Resource | Purpose |
|---|---|
| StatefulSet `aurcache-worker` | x86_64 workers, on `kubernetes.io/arch: amd64` nodes. One replica to start with. |
| Volume `data` (per replica) | `/var/lib/aurcache-worker`: the worker's identity, its SSH key, and the pool's mount point. 1 GiB. |
| Volume `pool` (per replica) | The storage pool, a raw block volume attached at `/dev/aurcache-pool`. 200 GiB. |
| Service `aurcache-workers` | Headless, only because a StatefulSet names one. Workers serve nothing. |

A worker is known to the server by its key, which it keeps on its `data`
volume. A StatefulSet keeps each replica's volumes through restarts and moves
to other nodes, so a replica stays the same approved worker. Scaling up adds
workers, each of which enrolls as a new one:

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
- `enableServiceLinks: false`, as the server has. Otherwise Kubernetes sets an
  `AURCACHE_<NAME>_PORT` variable for every Service in the namespace whose name
  starts with `aurcache`, and a Service called `aurcache-worker` would set
  `AURCACHE_WORKER_PORT`, which the server reads as its port number.

A pod's `resources` limits bound the worker and every build it runs. Within
them, the worker's [own limits](../workers/split-chroot.md#resource-limits)
decide which build gives way first. Keep `WORKER_TOTAL_BUILD_MEMORY_MAX` under
the pod's memory limit, so that a build over it fails with a reason rather than
the whole pod being killed.

A stopped worker's builds are requeued once their lease runs out, a minute by
default, and start again on another worker. To drain a node without losing
work, [stop the worker's intake](../workers/managing.md#stopping-a-workers-intake)
first:

```bash
aurcli worker pause <id> --wait && kubectl drain <node> --ignore-daemonsets
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
so that has to fit on the volume. To grow the pool, expand the volume first (if
the storage class allows volume expansion), then raise `WORKER_DISK_MAX` on the
worker's page; see [Resizing](../workers/storage-pool.md#resizing).

**Without raw block volumes**, uncomment `components/pool-image` in
`kubernetes/kustomization.yaml`. The pool is then a sparse image file on the
worker's `data` volume, which grows to 200 GiB to hold it, and is loop-mounted.
It works on any storage class, `local-path` included, at the cost of the loop
device and the nested filesystem. A pool moved from one backing to the other
starts empty; it only ever holds the base chroot and caches, which the worker
rebuilds.

## Other architectures

A chroot worker builds for the architecture of the node it runs on, so each
architecture is a StatefulSet of its own, pinned to nodes of that architecture.
`kubernetes/worker-arm64/` is the x86_64 one renamed, with its own labels and
volumes, on `kubernetes.io/arch: arm64` nodes; uncomment it in
`kubernetes/kustomization.yaml` to build aarch64 packages. The worker image is
multi-arch, so those nodes pull their own variant of the same tag. For armv7h,
copy that directory with `arm` and `armv7h` in place of `arm64` and `aarch64`.

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

The server writes the password into a `postgres://` URL as it is, so keep it to
letters, digits and `-_.~`. The data volume is still needed: the repository,
the worker CA and the build logs stay on it.

## Upgrading

The manifests use the `latest` tags, so Kubernetes pulls the image again when a
pod starts. Restart the server first, then the workers:

```bash
kubectl -n aurcache rollout restart deployment/aurcache
kubectl -n aurcache rollout status deployment/aurcache
kubectl -n aurcache rollout restart statefulset/aurcache-worker
```

To upgrade on purpose instead, pin a version in `kubernetes/kustomization.yaml`
(`images:` is there, commented) and change it when you mean to.

## The legacy container builder

The deprecated [legacy container builder](../workers/legacy-docker.md) runs each
build in a container it creates through a Docker API, on a Unix socket. A
Kubernetes node's runtime is containerd or CRI-O, which offer none, so it is not
part of these manifests.

The [hybrid image](../workers/hybrid.md) in its DinD mode (`privileged: true`,
`BUILD_ARTIFACT_DIR` unset) needs no outside daemon: builds run in a Podman
inside its own container, which a privileged pod can hold too. It is
deprecated, and needs the same privileges as the chroot worker while building
in a reused image rather than a clean chroot.

Against a separate Docker daemon, the only shape that works is a privileged
`docker:dind` container in the same pod, sharing the socket through an
`emptyDir`, with the build directory mounted **at the same path** in both
containers. The builder tells the daemon to bind `BUILD_ARTIFACT_DIR` into each
build container, and reads the results from the same directory itself, so the
path has to mean the same directory to both. This is not recommended: use the
chroot worker.

A builder that starts each build as an unprivileged pod of its own is not
available.

## Troubleshooting

**A worker's PVC stays `Pending`.** Its storage class cannot provision the
volume: often `volumeMode: Block`, which `local-path` and other host-path
provisioners do not support. Use `components/pool-image`, or name a class that
can in the `pool` claim template.

**The worker logs that it cannot open its storage pool.** The node's kernel is
older than 6.7, or has no btrfs. Run the worker on nodes that have both (a
`nodeSelector` or node affinity on the StatefulSet).

**The worker cannot connect: the certificate is not valid for the name.** The
host in the worker's `AURCACHE_URL` is not in the server's `AURCACHE_TLS_SANS`.
The manifests list `aurcache` and its namespaced forms in the `aurcache`
namespace; deploying to another namespace means changing those too.

**`Failed to create the directory $PKGDEST`, with the node's Docker or Podman
socket mounted into the pod.** The daemon binds `BUILD_ARTIFACT_DIR` from the
node's filesystem, and a volume mounted in the pod is not on the node at that
path, so each build container gets an empty or missing directory. Use the chroot
worker, or the same-path `docker:dind` shape above
([#200](https://github.com/Lukas-Heiligenbrunner/AURCache/issues/200)).
