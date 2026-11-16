# Kubernetes manifests

Plain manifests for running AURCache on Kubernetes, assembled with Kustomize
(built into `kubectl`). From the repository root:

```bash
kubectl apply -k kubernetes/
```

The [Kubernetes page](https://lukas-heiligenbrunner.github.io/AURCache/docs/setup/kubernetes)
of the documentation covers what the cluster needs, approving workers, exposing
the server, and the options below.

| Path | What it is |
|---|---|
| `kustomization.yaml` | The default set: namespace, server, x86_64 workers. Options are commented in it. |
| `namespace.yaml` | The `aurcache` namespace, labelled for privileged pods (the workers need them). |
| `server/` | The server Deployment, its data volume, nginx for the repository, and the `aurcache` Service (UI/API 8080, repository 8081, worker protocol 8083). |
| `worker/` | A StatefulSet of chroot build workers on amd64 nodes, each with its own data volume and a raw block volume for its storage pool. |
| `worker-arm64/` | The same workers on arm64 nodes, building aarch64 packages. |
| `components/pool-image/` | Storage pools in an image file on the data volume, for a storage class without raw block volumes. |
| `components/postgres/` | The server's database in an existing PostgreSQL instead of SQLite. |
