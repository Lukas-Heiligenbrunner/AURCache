---
sidebar_position: 8
---

# Demo worker

The demo instance (`compose/docker-compose.demo.yaml`) pairs the server with a
dummy worker that completes every job with synthetic near-empty packages after
`DEMO_BUILD_SECS` seconds — no compilation runs anywhere. It exists so visitors
can add AUR packages and watch dependency resolution and publishing happen in
seconds.

Deliberately the only published port is the web UI (`8080`): the pacman
repository is not exposed, because the demo's packages are synthetic and must
never be installed, and the worker protocol stays inside the compose network.

The dummy worker is I/O-free, so it runs unprivileged with a pinned
`WORKER_CONCURRENCY=4` to drain the demo queue quickly. Like a real worker it
enrolls through the shared `enroll` volume and keeps its identity in its data
volume.
