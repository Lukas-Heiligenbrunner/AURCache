---
sidebar_position: 1
---

# Build workers

A build worker is a long-lived process that enrolls with AURCache, claims build
jobs, builds each package, and uploads the result. Workers talk to the server
over mutual TLS on port 8083, so a worker can live in the same compose stack,
on another machine, or on another architecture.

| Worker | How it builds | Status |
|---|---|---|
| [Split chroot worker](./split-chroot.md) | Each package in its own `devtools` chroot | Supported — this is what every setup wants |
| [Legacy container builder](./legacy-docker.md) | Each package in a reused container | Deprecated, continuity only |
| [Demo worker](./demo.md) | Completes every job with synthetic packages | Demo instance only |
| [Hybrid image](./hybrid.md) | Server *and* a worker in one container | Deprecated, continuity only |

A worker starts from its environment variables. Its policy and tuning settings —
concurrency, build limits, cache budgets, timeouts — can then be set for it from
AURCache, on the worker's own page, and reach it without a restart. Everything
else (its identity, its paths, what a build may reach) stays on the machine.
See the [split chroot worker](./split-chroot.md) for the full reference and
[Managing workers](./managing.md) for day-to-day operation.
To write a worker of your own, see the [worker protocol](./protocol.md).
