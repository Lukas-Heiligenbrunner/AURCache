# The Kubernetes worker

Manifests for a worker that builds each package in a pod of its own, with no
privileged pod and no access to a node. Apply them after the server alone,
from the repository root:

```bash
kubectl apply -k kubernetes/server/
kubectl apply -k kubernetes/kube-worker/
```

`kubectl apply -k kubernetes/` is something else: the server with a privileged
chroot worker beside it, in a namespace raised to `privileged`. To have this
worker from that file instead, comment out `components/chroot-workers` in
`kubernetes/kustomization.yaml` and uncomment its `kube-worker` line.

This is a kustomization of its own: its resources live in two namespaces, and
name them.

## What it needs

- The server, running as the manifests in `kubernetes/server/` set it up: a Service
  `aurcache` in the namespace `aurcache`, whose pods carry the labels
  `app.kubernetes.io/name: aurcache` and `app.kubernetes.io/component: server`.
- A CNI that enforces NetworkPolicy (Calico, Cilium, kube-router, kindnet; not
  flannel alone). Without one, builds can reach whatever a pod in the cluster
  can. The worker tests this when it starts and logs an error if it can.
- A look at `networkpolicy.yaml` if the cluster runs NodeLocal DNSCache, has
  pods or nodes on public addresses (IPv6 above all), or an API server on a
  public endpoint: the policy cannot know those addresses, and says at each
  place what to change.
- A default StorageClass, or one named in `worker.yaml`, for a 1 Gi volume.
- Nodes labelled `kubernetes.io/arch` (every node is), for each architecture
  you build for.

## What it creates

| File | Purpose |
|---|---|
| `kustomization.yaml` | The default set. Options are commented in it. |
| `namespace.yaml` | The `aurcache-builds` namespace, where build pods run, labelled for the Pod Security `baseline` level. |
| `rbac.yaml` | The worker's ServiceAccount, its Role and RoleBinding (see below), and `aurcache-build`, the account build pods run as, which is bound to nothing. |
| `networkpolicy.yaml` | The firewall around build pods: DNS, the server's repository, and the internet outside private address ranges. |
| `worker.yaml` | The worker: a Deployment (one replica, `Recreate`, passing the `restricted` level) and the 1 Gi volume that holds its identity. |
| `resourcequota.yaml` | An optional ceiling on all builds together. Not applied unless enabled in `kustomization.yaml`. |

The worker lives in the `aurcache` namespace beside the server, so the
`aurcache-enrollment` Secret that approves workers automatically is the one the
server already reads.

## Permissions

The worker holds no cluster-wide role. In `aurcache-builds` it may create, get,
list, watch and delete pods (and delete them by label, to clean up after
itself), open exec sessions into them and read their logs. In `aurcache`, its
own namespace, it may do nothing: the image build pods take the build agent
from is named in `worker.yaml`, and `kustomization.yaml` keeps it equal to the
worker's own. Build pods get no token for the Kubernetes API at all.

## Running more than one

Never raise `replicas` above 1: a worker is known by its key, and two pods with
one key each report only their own builds. For more builds at once, raise
`WORKER_CONCURRENCY_DEFAULT`.

For a second worker, for another set of architectures or its own credentials,
copy `worker.yaml` under other names: the Deployment, and its volume claim,
which holds the worker's own key. It enrolls as a worker of its own. Give it a
builds namespace of its own too, with `WORKER_KUBE_NAMESPACE` naming it: copy
`namespace.yaml`, `networkpolicy.yaml`, the builds' ServiceAccount and the
dispatcher's ServiceAccount, Role and RoleBinding in `rbac.yaml` (one of each),
and the quota if you use one, for that namespace. Never let two dispatchers
share one: a Role cannot be limited to pods carrying a label, so each could
exec into the other's build pods, and use the SSH key the other's builds are
given while one of them runs.

## Checking a change

No cluster is needed to render the result:

```bash
kubectl kustomize kubernetes/kube-worker/
```
