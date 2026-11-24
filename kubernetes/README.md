# Kubernetes manifests

Plain manifests for running AURCache on Kubernetes, assembled with Kustomize
(built into `kubectl`). From the repository root:

```bash
kubectl apply -k kubernetes/              # the server and its workers
kubectl apply -k kubernetes/server/       # the server alone
kubectl apply -k kubernetes/kube-worker/  # after the server: the Kubernetes worker
```

The [Kubernetes page](https://lukas-heiligenbrunner.github.io/AURCache/docs/setup/kubernetes)
of the documentation covers what the cluster needs, approving workers, exposing
the server, and the options below.

| Path | What it is |
|---|---|
| `kustomization.yaml` | The server plus the workers you choose, as components. Options are commented in it. |
| `server/` | The base: the `aurcache` namespace (Pod Security `baseline`), the server Deployment, its data volume, nginx for the repository, and the `aurcache` Service (UI/API 8080, repository 8081, worker protocol 8083). |
| `chroot-worker/` | A StatefulSet of privileged chroot build workers on amd64 nodes, each with its own data volume and a raw block volume for its storage pool. |
| `chroot-worker-arm64/` | The same workers on arm64 nodes, building aarch64 packages. |
| `components/chroot-workers` | Adds `chroot-worker/` and raises the namespace to Pod Security `privileged`. On by default. |
| `components/chroot-workers-arm64` | The same for `chroot-worker-arm64/`. |
| `components/pool-image` | The chroot workers' storage pools in an image file on their data volume, for a storage class without raw block volumes. |
| `components/chroot-worker-egress` | A NetworkPolicy keeping the chroot workers' builds off private addresses. |
| `components/postgres` | The server's database in an existing PostgreSQL instead of SQLite. |
| `kube-worker/` | The Kubernetes worker: an unprivileged dispatcher beside the server that builds each package in a pod of its own, in the `aurcache-builds` namespace. A kustomization of its own, applied after `server/` or enabled in `kustomization.yaml`. |
