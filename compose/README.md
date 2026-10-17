# Compose files

All Docker Compose topologies live here. Run them from the repository root,
e.g. `docker compose -f compose/docker-compose.yaml up -d`. Their build
contexts point at the root (`context: ..`), so the root stays the project
directory either way.

## User-facing

| File | Purpose |
|---|---|
| `docker-compose.yaml` | Bundled single-host setup (the turnkey default): server + one local build worker that auto-enrolls through a shared volume. From a checkout: `docker compose -f compose/docker-compose.yaml up -d`. Standalone: `curl -O https://raw.githubusercontent.com/Lukas-Heiligenbrunner/AURCache/master/compose/docker-compose.yaml && docker compose up -d`. |
| `docker-compose.remote-worker.yaml` | Worker for separate hardware or a foreign architecture (e.g. aarch64). No shared enroll volume, so trust is established explicitly (CA pin, pre-approved fingerprint, token, or UI approval). |

## Local development

| File | Purpose |
|---|---|
| `docker-compose.local.yaml` | Same topology as the turnkey file, running the published ghcr.io images; ports shifted +10 so it runs alongside a dev server or an e2e run. |
| `docker-compose.demo.yaml` | Public playground: server + dummy worker completing jobs with synthetic packages + PostgreSQL. No real builds; the repo port is unpublished on purpose. |
| `docker-compose.hostmode.dev.yaml` | Legacy dev setup that mounts the host Docker socket. |
| `docker-compose.dindmode.dev.yaml` | Simpler legacy dev setup. |

## End-to-end tests (driven by `scripts/`)

| File | Purpose |
|---|---|
| `docker-compose.e2e.yaml` | Server + one real privileged build worker; the default target of `scripts/test-e2e.sh`. |
| `docker-compose.e2e-ssh.yaml` | Overlay applied on top of the e2e file by `scripts/test-e2e-ssh.sh`; adds a throwaway git server for the SSH-credential test. |
| `docker-compose.e2e-hybrid.yaml` | Single-container hybrid image shaped like a pre-worker deployment; asserts upgrades keep building with no edits (`scripts/test-e2e-hybrid.sh`). |
| `docker-compose.e2e-hybrid-legacy.yaml` | Hybrid image with the Docker socket mounted, selecting the deprecated legacy container builder (`scripts/test-e2e-hybrid-legacy.sh`). |
