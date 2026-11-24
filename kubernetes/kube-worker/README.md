# The Kubernetes worker

Manifests for a worker that builds each package in a pod of its own. Apply
them after the server, from the repository root:

```bash
kubectl apply -k kubernetes/server/
kubectl apply -k kubernetes/kube-worker/
```

What the cluster needs, what each file creates, its permissions and how to run
a second worker are in `docs/docs/workers/kubernetes.md`
([online](https://lukas-heiligenbrunner.github.io/AURCache/docs/workers/kubernetes)).
