---
sidebar_position: 1
---

# Docker images

## Tags

| Image | What it is |
|---|---|
| `aurcache-server` | The server. This is what most setups want. |
| `aurcache-worker` | A build worker. Multi-arch (`amd64`, `arm64`, `arm/v7`). |
| `aurcache` | The hybrid compatibility image — **deprecated**, see [Hybrid](../../workers/hybrid.md). |
| `aurcache-builder-v2` | Spawned per build by the docker worker, which the hybrid image runs. Not used by the split setup. |
| `aurcache-builder` | The image AURCache 0.5.0 builds in. No longer published; 0.5.0 keeps pulling the last one. |

Each is published as `:<version>` and `:latest` for a release tag, and as
`:git` for the current master branch, which is also rebuilt weekly.
`aurcache-builder-v2:latest` is the newest build of either, so a worker
from master finds one before the first release that ships it.

The worker image is multi-arch: the same tag serves `linux/amd64`,
`linux/arm64` and `linux/arm/v7`, and a foreign-architecture worker is the same
image run emulated (see [Cross-arch builds](../../workers/split-chroot.md#building-for-another-architecture)).
The server image is multi-arch too, so it runs on e.g. a Raspberry Pi.

## Compose files

The repo ships several topologies under `compose/`:

| File | Use |
|---|---|
| `docker-compose.yaml` | The bundled single-host setup: server + one local worker. The turnkey default. |
| `docker-compose.remote-worker.yaml` | A worker for separate hardware or a foreign architecture, joining a server elsewhere. |
| `docker-compose.demo.yaml` | Public demo: server + dummy worker, no real builds. |
| `docker-compose.e2e*.yaml` | End-to-end test topologies. Not for deployment. |

`aurcli setup compose` writes these topologies with the same comments and
defaults — see [Docker Compose setup](../docker.md#generating-these-files).
