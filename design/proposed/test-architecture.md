# Test architecture and repository layout

Which tier a test belongs to, how the end-to-end suites share one harness
instead of one script per concern, and a repository organised by purpose:
what users deploy, what the product is built from, and what tests it each
have their own tree, subdivided by kind only inside it.

`e2e-scenarios.md` defines the harness's contracts (topology, scenario,
fixture, helpers) and stays the reference for them; it is updated in the same
PR as this design, so the two use the same paths, topology names and
contract. This design adds the tiers around that harness, the Kubernetes
topology, a deliberate CI matrix, the review of duplicated coverage, the
repository layout and the migration order. `e2e-scenarios.md` points here for
the last two.

Status: **Proposed** · Last updated: 2026-11-19

---

## Decided

- **Purpose first.** Top-level directories are named for a purpose (deploy,
  images, packaging, tests, docs), and subdivided by kind inside one where
  useful. See [Layout](#layout).
- **`tests/` at the repository root**, holding every test that is not
  `cargo test`.
- **A fix brings no e2e script, compose file or fixture image of its own.**
  Its regression is a unit test, a leg of an existing component suite, or a
  scenario of the e2e harness. See [Tiers](#tiers-and-where-a-test-goes).
- **The `.dev.yaml` compose files go.** The hybrid topologies' files replace
  them, and `just up <topology>` starts any topology for development. See
  [Starting a topology for development](#starting-a-topology-for-development).
- **The reference topology is the user compose file plus a test overlay**,
  which builds under local test tags, never the release ones.
- **CI uses a Cargo cache**, with `Swatinem/rust-cache`, saved only from
  `master`. See [CI compilation](#ci-compilation).
- **One `run-e2e` label** runs the nightly matrix on a pull request, and
  **`run-e2e:<topology>`** one column. `run-e2e-kube` goes.
- **The hybrid topologies run weekly**, not nightly.
- **The isolation suite's unconfined control is stock `makechrootpkg`** for
  as long as devtools ships it.
- **The demo compose file moves without a copy.** Nothing outside the
  repository reads `compose/docker-compose.demo.yaml` by path.

## Inventory

Runtimes are CI wall-clock from the master run of 2026-11-18 (`67c1ca47`)
unless marked local. "Open PR" rows describe the PR's head as of the date
above.

### Every push (`ci.yml`)

| Job / suite | Covers | Runtime | Uses |
|---|---|---|---|
| `lintAndTestRust`: `cargo test --all`, `cargo test -p aurcache-api --features static` | Unit and integration tests of the backend workspace, including the hermetic fake worker (`aurcache-worker/tests/fake_worker.rs`), the worker-protocol fixtures and PKGBUILD parsing under the sandbox | 17 min job, of which 8 min test step; the tests themselves ~8 s locally | `aurcache-common/tests/fixtures/worker-protocol/`; the `alpm-pkgbuild-bridge` script downloaded from GitLab |
| same job: "Test PKGBUILD parsing as root" | The parse tests again as root, with and without Landlock, as the images and a Landlock-less kernel run them | 1 min | the test binary built above |
| same job: package build shapes, driver-free types | `cargo check` of each exact package and image invocation | 2 min, in the 17 | -- |
| `lintAndTestFrontend` | fmt, clippy for wasm and host, `cargo test` | 3 min | -- |
| `browserTest`: `scripts/test-frontend.sh` | Every route mounts in Chrome against a seeded server with the UI embedded | 9 min (~1 min locally, warm) | `scripts/fixtures/frontend-seed.sql` |
| `kernelTests`: `scripts/test-kernel.sh` | Storage pool (loop, btrfs simple quotas), pool drain, cache volumes, cgroup peak, as root in a privileged container | 12.5 min, nearly all of it compiling test binaries | `archlinux/archlinux` image |
| `sandboxTest`: `scripts/test-sandbox.sh` | The Landlock confinement around a PKGBUILD sourced outside the chroot, in both polarities: denied through `aurcache-build`, escaping through stock `makechrootpkg`; in the same two legs, that the build's system upgrade never sees AURCache's repository (a `replaces=` bait in it) | 3 min | Dockerfile generated inline; `docker/nspawn-wrapper.sh`, `packaging/patch-makechrootpkg.py`, `packaging/sandbox-protected`, `docker/testpkg/hostile-fixture/` |

No job caches Cargo output: each compiles its part of the workspace from
scratch.

### Nightly, when something landed in 25 h, or on the `run-e2e` label

| Suite | Covers | Runtime | Uses |
|---|---|---|---|
| `scripts/test-e2e.sh hello` | Server and one chroot worker from this tree: enrollment, a real AUR build, disk usage reported, `pacman -S` from the repository | 22-29 min in CI (image builds dominate); ~4 min locally with warm images | `compose/docker-compose.e2e.yaml`, `docker/server.Dockerfile`, `docker/arch.Dockerfile` (`worker`), `scripts/lib/e2e.sh`, the AUR and Arch mirrors |

### In no workflow

| Suite | Covers | Uses |
|---|---|---|
| `scripts/test-e2e-hybrid.sh` | The hybrid image in DinD mode: embedded worker enrolls over loopback, Podman inside the container, the Docker worker builds | `compose/docker-compose.e2e-hybrid.yaml`, `docker/arch.Dockerfile` (`hybrid`); `exec`s `scripts/test-e2e.sh` |
| `scripts/test-e2e-hybrid-legacy.sh` | The same image with the host's Docker socket: the build directory shared with an outside daemon | `compose/docker-compose.e2e-hybrid-legacy.yaml`, `docker/builder.Dockerfile`, `/tmp/aurcache-legacy-builds` on the host; `exec`s `scripts/test-e2e.sh` |
| `scripts/test-e2e-ssh.sh` | Build credentials reach the build user inside the chroot: unauthorised fails, authorised succeeds with the payload's marker installed | `compose/docker-compose.e2e.yaml` + `docker-compose.e2e-ssh.yaml`, `scripts/lib/e2e.sh`, `docker/git-ssh.Dockerfile`, `docker/git-ssh-entrypoint.sh`, `docker/testpkg/ssh-fixture/` |
| `scripts/update-website-screenshots.sh`, `scripts/showcase/` | Not tests: website screenshots from a restored snapshot. `showcase/generate.sh` builds one real package in `compose/docker-compose.e2e.yaml` | `scripts/showcase/snapshot/` |
| `conformance-tests.md` | Proposed, not built: black-box checks in each PKGBUILD's `check()` | -- |

### Open PRs

| PR | Test additions |
|---|---|
| #435 | `aurcache-utils/tests/ssh_clone.rs` (server clones over SSH from a throwaway `sshd`, per push); a third `test-e2e-ssh.sh` phase for the same server-side clone through the API; `docker/testpkg/ssh-source-fixture/`; changes to `git-ssh.Dockerfile` and its entrypoint |
| #438 | The Kubernetes worker: `kubernetes/kube-worker/`, `docker/kube-worker.Dockerfile`, `docker/build-kube-worker.sh`. Built on #446, whose test additions it carries. |
| #446 | `scripts/test-build-agent.sh`, a new per-push job (~3 min): the static build agent in the builder image under a build container's flags; legs: `hello` builds, a hostile build (root, the SSH key, the pacman helper), stop by SIGKILL/SIGTERM, arm64 under qemu where registered. `docker/testpkg/agent-hostile/`, `docker/testpkg/agent-thief/`, `scripts/fixtures/build-agent/key-scan.pl`, `git-ssh.Dockerfile` reused. Hybrid-legacy now runs on the agent, and the Docker worker reports disk usage. |
| #447 | `scripts/test-e2e-kube.sh` (1375 lines, ~20 min locally, ~10 GB) and `e2e-kube.yml`, nightly and on a `run-e2e-kube` label. Eleven legs: enrollment, NetworkPolicy, ResourceQuota, a dependency only in AURCache's repository, SSH through the agent with a thief dependency, cancel, worker killed, worker pod deleted, pod that cannot start, a 2 GiB package, user namespaces, policy removed. `scripts/fixtures/kube-e2e/` (overlays, gitssh, five packages), `docker/testpkg/big/`. Deploys `kubernetes/server/` and `kubernetes/kube-worker/` as shipped: `kube-e2e/kustomization.yaml` names them as `../../../kubernetes/server` and `../../../kubernetes/kube-worker`. |
| #450 | A `test-sandbox.sh` leg: a build not offered the worker's ssh-agent is refused its socket (Landlock `RESOLVE_UNIX`, Linux 7.1). A third `test-e2e-ssh.sh` phase: a package not in `WORKER_GIT_SSH_PACKAGES` gets no key. Unit tests for the key request, the offer and the package ceiling. |
| #452 | Two `test-e2e-ssh.sh` phases: `WORKER_SSH_KNOWN_HOSTS` naming the server builds, naming another host is refused. Unit tests for staging and binding `known_hosts`. |

With #435, #450 and #452 merged, `test-e2e-ssh.sh` has six phases; the two
on master each start from `down -v`, so a fresh worker volume and base
chroot.

### Where test files are

Test-only files sit in five places: `docker/testpkg/` (three package
fixtures on master, seven once the PRs land), `docker/git-ssh*`,
`compose/docker-compose.e2e*.yaml` (four), `scripts/fixtures/` (the frontend
seed on master; `build-agent/` with #446, `kube-e2e/` with #447), and
Dockerfiles written inline by `test-sandbox.sh`. Hostile packages exist three
times (`hostile-fixture`, `agent-hostile`/`agent-thief`,
`kube-e2e-ssh`/`kube-e2e-thief`), each with its own idea of what to try.

## Duplicated and redundant work

| # | Finding | Recommendation | Saving |
|---|---|---|---|
| D1 | `compose/docker-compose.{hostmode,dindmode}.dev.yaml` are the two hybrid e2e files with fixed ports, `LOG_LEVEL=debug` and bind-mounted data. | **Drop** them; the topology files are the only copy, started with `just up hybrid` or `just up hybrid-host`. | Two files that drift |
| D2 | `test-e2e-hybrid.sh` and `-hybrid-legacy.sh` run the same image, the same Docker worker and the same scenario; only the daemon's location differs. Neither runs in CI. | **Keep both as topologies** (each is a distinct upgrade path), **drop both scripts**. Run `basic` on each weekly; one run builds the hybrid image once for both. | One image build (cached layers aside) and two wrapper scripts; coverage goes from none to weekly |
| D3 | `docker-compose.e2e.yaml` is a second copy of the turnkey stack, already different from it: the repository served by the server rather than the `repo` nginx service, `WORKER_CONCURRENCY` rather than `WORKER_CONCURRENCY_DEFAULT`, another network name. The nginx service users run is tested nowhere. | **Merge**: the reference topology is `deploy/compose/docker-compose.yaml` plus a test overlay (build from the tree under local tags, ports, the env hooks), as #447 deploys the shipped manifests plus overlays. | One file, and coverage of what users deploy |
| D4 | #435's server-clone phase of `test-e2e-ssh.sh` repeats `ssh_clone.rs`, which clones with a real `sshd` per push. What the phase adds is the refusal message through the API. | **Drop** the phase; assert the message in an API test. | One stack cycle (estimate 3-5 min with a fresh worker) |
| D5 | #450's "not listed" phase repeats the ceiling's unit tests (`unset_is_every_package`, `empty_lists_no_package`, `membership_is_exact`); its enforcement is the sandbox leg's. | **Drop** the phase; assert the log line in the worker's unit test. | One stack cycle |
| D6 | #452's "unknown host" phase tests OpenSSH's host-key check once the file is used; `git_ssh_command_uses_the_agent_and_sets_host_policy` and the staging tests show it is used. The "pinned host" phase is the one that catches the readability bug the PR fixes. | **Keep** pinned host, folded into `ssh-credentials` (below). **Drop** unknown host. | One stack cycle |
| D7 | The remaining SSH phases recreate the whole stack between phases. | **Merge** into one `ssh-credentials` scenario (unauthorised, then authorised with a pinned `known_hosts`), recreating workers only (`e2e-worker-env`), as `e2e-scenarios.md` specifies. | Six fresh stacks become one stack and one worker recreate |
| D8 | Three hostile fixture sets (`hostile-fixture`, `agent-hostile`+`agent-thief`, `kube-e2e-ssh`+`kube-e2e-thief`) each encode what a hostile build tries. `key-scan.pl` exists only for the build agent. | **Merge** into one fixture set under `tests/fixtures/packages/hostile*` with one list of attempts; every executor's suite runs the same set. Keep the per-executor legs: the key sits in a different place for each. | Fixture drift; the next executor gets the attempts for free |
| D9 | Kube's dependency-from-repository, cancel and worker-restart legs are meaningful on every topology, and the chroot worker has no e2e for any of them. | **Merge** into shared scenarios (`repo-dependency`, `cancel`, `worker-restart`) run on the reference topology and on `kube`. `worker-restart` requires no capability; its check that the source cache survived is one line conditional on `chroot`. Keep the kube-only legs (policy, quota, stop, stuck pod, 2 GiB relay, user namespaces, open policy) as scenarios requiring kube capabilities. | No time; three scenarios written once instead of per executor |
| D10 | `kernelTests` compiles the four test binaries that `lintAndTestRust` compiles too; the tests run in seconds. | **Merge**: run them in the Rust job from that job's build. `test-kernel.sh` takes the executables from `cargo test --workspace --no-run` there rather than `-p`, whose different feature set would recompile. | ~12 runner-minutes per push; the Rust job grows ~1-2 min (pacman in the container plus the tests) |
| D11 | Every compiling job starts from an empty `target/`. | **Cache** Cargo output per job ([CI compilation](#ci-compilation)). | Most of each job's compile time on a warm cache (estimate: the Rust job's 17 min to 6-8) |
| D12 | With #447, the nightly builds the server image twice, in `e2e-nightly.yml` and `e2e-kube.yml`. | **Merge** into one e2e workflow: an image job builds and saves each image once, topology jobs load them. | One server image build per night (estimate 5-8 min of runner time) |
| D13 | `conformance-tests.md` proposes sandbox checks that "mirror `test-sandbox.sh`". | **Keep**, but run the isolation suite's probe against the packaged binary instead of copying its cases. | A second copy avoided |
| D14 | `fake_worker.rs` and the e2e both see enrollment, claim and complete. | **Keep** both: the fake worker asserts the protocol's safety rails offline, the e2e a real build. Scenarios must not re-assert protocol details. | -- |
| D15 | #450's `the_agent_socket_is_granted_only_to_a_build_offered_it` builds the ruleset; the sandbox leg proves the kernel enforces it. | **Keep** both: different layers. | -- |

What CI runs too often: only D10-D12, all compilation and image building
repeated across jobs. No suite runs more often than its tier warrants. What
it runs too rarely: the hybrid topologies and the SSH test, in no workflow.

## Tiers, and where a test goes

1. **Unit and integration** (`cargo test`, both workspaces). In the owning
   crate, offline, no root. Every push.
2. **Component suites**: one per boundary where AURCache drives real tools
   or the kernel without a server. Each owns one interface and asserts
   through it:
   - `kernel`: the pool and cgroups (`aurcache-chroot`, the worker's
     volumes);
   - `build-isolation`: a hostile build stays confined, run through each
     executor's build entry point: `aurcache-build --interface=N` for the
     chroot worker (today's `test-sandbox.sh`), the build agent in a build
     container (today's `test-build-agent.sh`). One fixture set, one leg per
     executor;
   - `browser`: the UI boots.

   Every push. A component suite grows legs; a new suite is only for a new
   boundary, such as a new executor's entry point.
3. **End to end**: one harness, topologies × scenarios. Nightly (the hybrid
   topologies weekly), and on the `run-e2e` labels.

The rule for a regression test: **the lowest tier at which it would have
failed before the fix.**

- Decided by a function given inputs (a fake database, a fake cluster, a
  parsed spec)? A unit test in the owning crate.
- Needs a real kernel, container runtime, OpenSSH or devtools, but no
  server? A leg in the component suite owning that boundary.
- Needs a server, a worker and the network between them? A scenario file
  in `tests/e2e/scenarios/`, with `requires:` for what it needs from the
  topology.
- Needs something only one deployment has (a NetworkPolicy)? Still a
  scenario, requiring a capability that topology declares.

**A fix never adds an e2e script, a compose file or a fixture image.** What
its test needs beyond the existing ones is a change to them:

- a step the scenarios cannot express is a new helper in `tests/e2e/bin/`;
- a fact a scenario cannot see is a CLI or API field, as `e2e-scenarios.md`
  requires;
- a service the fixtures lack extends an existing fixture: another
  repository or key in `gitssh`, another package in
  `tests/fixtures/packages/`;
- a deployment detail the topologies lack extends an existing topology's
  overlay, or a capability it declares.

A new fixture, topology or component suite is a design decision, argued for
by the external service, deployment or boundary it adds and recorded in this
document, never by the fix that wanted it.

## End to end: topologies × scenarios

### Topologies

| Topology | Stack | Workers | Capabilities | Built from |
|---|---|---|---|---|
| `chroot` (reference) | compose | 1 chroot worker | `chroot disk-usage kept-tree` | `deploy/compose/docker-compose.yaml` + overlay |
| `chroot-2` | compose | 2 chroot workers | + `multi-worker` | same, second worker in the overlay |
| `hybrid` | compose | Docker worker in the server container, Podman inside | `container disk-usage` | own file (shaped like a pre-worker deployment on purpose) |
| `hybrid-host` | compose | same, host's Docker socket | `container disk-usage` | own file; `setup.sh` builds the builder image |
| `kube` | kind | Kubernetes worker, build pods | `container disk-usage network-policy quota pod-lifecycle` | `deploy/kubernetes/server/` + `deploy/kubernetes/kube-worker/` + overlays |

`disk-usage` on the Docker and Kubernetes workers assumes #446, with which
the build agent reports it; on master the Docker worker reports none.

The `chroot` overlay adds `build:` from this tree to each AURCache service of
the user file, and the ports and env hooks the topology contract requires.
It also sets each built service's `image:` to a local test tag
(`aurcache-e2e/server:test`, `aurcache-e2e/worker:test`). Compose merges an
overlay's `build:` into the service and keeps the user file's
`image: ghcr.io/lukas-heiligenbrunner/aurcache-*:latest`, so without the tag
`docker compose build` would name a test build from the tree as the release
image on that machine: the next `docker compose up` of the real file there
would run it, and a push would publish it. (`image: !reset null` also drops
the name, leaving Compose's `<project>-<service>`; a fixed tag reads better
in `docker images`.) `run.sh` and `up.sh` refuse to start a merged
configuration (`docker compose config --images`) that names a `ghcr.io`
image for a service it builds, so an overlay that forgets the tag fails
before it builds anything.

Built from the user file, the repository is the `repo` nginx service, at
`http://repo` on that file's `aurcache` network, not `http://aurcache:8081`
on `aurcache_network`; the topology declares both (`E2E_REPO_URL`,
`E2E_NETWORK`), and `e2e-install` and the fixtures read them.

`kube` needs the topology contract to stop assuming compose. A topology
directory holds `topology.env` and provides five hooks: `up`, `down`,
`logs <worker>`, `worker-env` and `worker-kill <worker>`. A compose
topology lists its files in `E2E_COMPOSE_FILES` and takes the hooks from
`tests/e2e/topologies/lib/compose.sh`, which implements them once; `kube/`
has a `hooks.sh` implementing them with `kind`, `kubectl` and kustomize.
Scenarios see only `aurcli`, `jq` and the `e2e-*` helpers, as before;
`e2e-worker-kill` is the new helper, which the worker-restart and crash
scenarios need. `e2e-scenarios.md`'s topology contract states this in full.

The build runner's parity step (`build-runner.md`, step 1) is not a
topology: it is the `chroot` topology with the runner selected through
`e2e-worker-env`, and a scenario comparing the two packages.

### Starting a topology for development

```bash
just up chroot                  # tests/e2e/up.sh chroot
just up hybrid-host             # what hostmode.dev.yaml was
AURCACHE_PORT=8090 just up kube # ports move together, as in the e2e
just down hybrid-host           # keeps the volumes; --fresh drops them
```

`up.sh` builds the topology's images from this tree, starts it, waits for
the fleet as the harness does, and prints the UI and repository URLs. It
runs no scenario and leaves the stack up. Data lives in the topology's named
volumes and survives `down` unless `--fresh` is given. `up.sh` and `run.sh`
share the topology hooks, so the stack a developer pokes at is the one a
scenario runs against, and `REUSE=1 tests/e2e/run.sh <scenario>` runs a
scenario against it.

### The CI matrix

Each scenario's header gains `ci:` next to `requires:`. `requires:` says
where it is meaningful; `ci:` says where CI runs it. The nightly runs:

- every scenario on the reference topology, `chroot`, unless its
  requirements exclude it;
- `basic` (`ci: all`) on `chroot-2` and `kube`;
- a scenario whose requirements only one topology meets (`network-policy`,
  `multi-worker`) on that topology;
- anything else only where `ci:` names it.

A weekly run covers the hybrid columns: `basic` on `hybrid` and
`hybrid-host`. For the eleven scenarios in `e2e-scenarios.md` (the shared
`repo-dependency`, `cancel` and `worker-restart` among them) and the seven
kube-only legs, the full product over five topologies is 90 runs; this
matrix is about 27 across the nightly and weekly runs, and adding a shared
scenario adds one run, not five.

One e2e workflow holds all of it: an image job builds each image once and
saves it as an artifact, and one job per topology loads them and runs its
cells in sequence, the jobs in parallel, so a run takes as long as its
slowest column. On a pull request, `run-e2e` runs the nightly matrix and
`run-e2e:<topology>` one column, hybrid ones included; each label is removed
when its run ends.

### Alternatives

- **A script per concern** (today). Each copies the stack handling, the
  hostile fixtures and the readiness loops, and three of them run nowhere.
  Rejected.
- **The full product nightly.** Five times the runner time, and the AUR and
  mirror flakiness multiplied with it, to rerun scenarios whose outcome does
  not depend on the topology. Rejected.
- **A suite per executor**, as #446 and #447 are shaping up. Cancel,
  restart and the repository dependency would be written once per executor,
  and the chroot worker has none of them. Rejected.
- **A Rust harness now** (an `E2eStack` fixture over `aurcache-client`).
  The kube legs that kill a worker mid-build are the fault injection
  `e2e-scenarios.md` names as the trigger. A `worker-kill` hook covers what
  they need, so bash stays for now; scenarios written as `aurcli` commands
  port mechanically when a scenario needs precise timing.
- **Recommended:** the topology × scenario harness, with the kube topology,
  the `ci:` matrix and the tier rule.

## CI compilation

`Swatinem/rust-cache` in each job that compiles, with its defaults kept
where they matter for concurrency:

- **One key per job.** The default key includes the job id, the toolchain
  and a hash of `Cargo.lock`. Jobs that build different feature sets or
  profiles (the `static` server, the release sandbox, clippy for wasm) never
  share an entry, so none restores another's half-matching `target/`.
  `workspaces:` names `backend -> target` or `frontend-rs -> target`.
- **Saved only from `master`.** `save-if: ${{ github.ref ==
  'refs/heads/master' }}`. Pull requests restore `master`'s entry and save
  nothing, so a PR cannot fill the 10 GB repository quota with entries no
  one reuses, or evict `master`'s.
- **No race to corrupt.** Restoring downloads into the job's own runner;
  nothing is shared at run time. An Actions cache entry is immutable once
  saved: two `master` runs finishing together both try to reserve the same
  key, one saves and the other skips with a warning. The next `Cargo.lock`
  or toolchain change makes a new key, and the old entry ages out.
- **Dependencies, not the workspace.** The action cleans the workspace's own
  crates before saving, so the entry holds what rarely changes and stays
  small. The compiling jobs also set `CARGO_PROFILE_DEV_DEBUG=0`: debug info
  is a large share of a debug build's artifacts, and no CI job reads it.
- **Root-written files out first.** The kernel tests, once they run in the
  Rust job (D10), write pool images as root under `CARGO_TARGET_TMPDIR`; the
  step removes that directory before the cache's post step archives
  `target/`.

`actions/cache` keyed by hand could do the same, with the save gating, the
pruning and the key written out in every job; the action is those defaults.
The e2e image job reads buildx's `gha` cache from the scopes the publish
workflow already fills (`build-images.sh --cache gha`, one scope per image)
and writes none: its amd64 layers are a subset of the publish run's, and an
amd64-only save under the same scope would replace the multi-platform cache
the publish run depends on.

### Size against the quota

An estimate, which step 4's first `master` runs measure (`gh cache list`):

- **Per set of entries.** `backend/Cargo.lock` has 655 crates and
  `frontend-rs/Cargo.lock` 411. Dependencies only, without debug info and
  compressed, that is roughly 1-1.5 GB for the Rust job (the test build, the
  `static` server and the package-shape checks), 0.5-1 GB each for the
  browser and frontend jobs, and a few hundred MB together for the sandbox
  and build-agent jobs: about 3 GB for the five compiling jobs, the kernel
  job having merged into the Rust job (D10).
- **`Cargo.lock` churn.** The lock files changed 14 times on `master` in the
  30 days to 2026-11-18: a new set of keys every two days or so. Only the
  newest set is restored: a PR whose lock has no exact entry restores by the
  key's prefix, which picks the most recent. GitHub evicts the least recently
  used entries once a repository is over quota, and any entry not restored
  for 7 days, so superseded sets are the first to go and the working set is
  one set, ~3 GB.
- **What it shares the quota with.** The publish workflow's buildx cache
  already fills it: on 2026-11-19 the repository's entries totalled
  10.55 GB (`/actions/cache/usage`), 10.1 GB of them buildx blobs saved from
  `master` with `mode=max` over three platforms. Eviction is by last use
  across both, and both are restored on every `master` push, so what goes
  first is what no current build restores: blobs left from earlier builds
  of the images, and superseded Cargo sets. The Cargo set therefore
  displaces about 3 GB of stale buildx blobs, not the ones the next publish
  run needs, as long as the two current sets fit together.

Step 4 checks this: after a week, the publish run's image builds still
report their expensive stages `CACHED` (the armv7 toolchain above all). If
they stop doing so, the Cargo cache shrinks to the Rust job's entry alone,
~1.5 GB and most of the saving, before anything else changes.

## Layout

### Purpose first

Each top-level directory answers one question; inside it, files are grouped
by kind where that helps. A file used by one suite lives in that suite's
tree; `tests/fixtures/` holds only what several suites use.

```
backend/  frontend-rs/  assets/   the product's source
deploy/                           running AURCache
  compose/                        docker-compose.yaml, docker-compose.remote-worker.yaml,
                                  docker-compose.demo.yaml (build context `../..`), README.md
  kubernetes/                     what kubernetes/ holds today
images/                           the product's container images (today's docker/)
  server.Dockerfile  arch.Dockerfile  builder.Dockerfile  demo-worker.Dockerfile
  kube-worker.Dockerfile          from #438
  entrypoints, build-time scripts (build-rust.sh, build-kube-worker.sh, ...),
  podman and pacman config
  build.sh                        today's scripts/build-images.sh
packaging/                        the Arch packages: AUR submodules, units, install-files.sh,
                                  patch-makechrootpkg.py, sandbox-protected, ...
  build-packages.sh               today's scripts/build-packages.sh
tests/                            every test that is not `cargo test`
  README.md                       the tiers, the rule, how to run each
  fixtures/                       used by several suites
    packages/                     hello, hostile*, ssh*, big, ...
    images/gitssh/                Dockerfile, entrypoint.sh: the git server behind SSH
                                  (e2e, build-isolation, kube)
  kernel/run.sh
  build-isolation/                run.sh (every leg), chroot/ (run.sh, probe, image),
                                  agent/ (run.sh, key-scan.pl)
  browser/                        run.sh, seed.sql (today's frontend-seed.sql)
  e2e/
    run.sh  up.sh  lib/  bin/
    topologies/
      lib/compose.sh              the five hooks, once, for every compose topology
      chroot/  chroot-2/          topology.env, overlay.yaml over deploy/compose/
      hybrid/  hybrid-host/       topology.env, compose.yaml; hybrid-host also setup.sh
      kube/                       topology.env, hooks.sh, kustomize overlays
    fixtures/gitssh/              compose.yaml, setup.sh (kube: a kustomize component),
                                  running tests/fixtures/images/gitssh
    scenarios/*.sh
docs/                             the website
  screenshots/                    update.sh (today's update-website-screenshots.sh), showcase/
design/                           unchanged
tools/lib/common.sh               shell helpers tests, images and packaging all source
```

`tests/e2e/fixtures/` holds one directory per external service a scenario
talks to; `gitssh` is the only one. It is not where a fix puts what its test
needs: per [Tiers](#tiers-and-where-a-test-goes), that extends `gitssh` or a
topology.

Where today's contents go:

| Today | Goes to |
|---|---|
| `compose/docker-compose{,.remote-worker,.demo}.yaml`, `compose/README.md` | `deploy/compose/` |
| `compose/docker-compose.e2e.yaml` | replaced by `tests/e2e/topologies/chroot/overlay.yaml` over the user file (D3) |
| `compose/docker-compose.e2e-hybrid{,-legacy}.yaml` | `tests/e2e/topologies/hybrid{,-host}/compose.yaml`; their `context: ..` becomes `../../../..` |
| `compose/docker-compose.e2e-ssh.yaml` | `tests/e2e/fixtures/gitssh/compose.yaml` |
| `compose/docker-compose.*.dev.yaml` | removed (D1) |
| `kubernetes/` | `deploy/kubernetes/` |
| `docker/*.Dockerfile`, entrypoints, image build scripts, configs | `images/` |
| `docker/testpkg/` | `tests/fixtures/packages/` |
| `docker/git-ssh.Dockerfile`, `docker/git-ssh-entrypoint.sh` | `tests/fixtures/images/gitssh/` |
| `scripts/fixtures/frontend-seed.sql` | `tests/browser/seed.sql` |
| `scripts/fixtures/build-agent/` | `tests/build-isolation/agent/` |
| `scripts/fixtures/kube-e2e/` | `tests/e2e/topologies/kube/`; its packages join `tests/fixtures/packages/` in steps 9 and 10, its `gitssh.yaml` becomes the `gitssh` fixture's component in step 9 |
| `scripts/test-kernel.sh`, `test-frontend.sh` | `tests/{kernel,browser}/run.sh` |
| `scripts/test-sandbox.sh`, `test-build-agent.sh` | `tests/build-isolation/{chroot,agent}/run.sh` |
| `scripts/test-e2e*.sh` | replaced by `tests/e2e/run.sh` and scenarios |
| `scripts/lib/e2e.sh` | `tests/e2e/lib/e2e.sh` (step 6) |
| `scripts/lib/common.sh` | `tools/lib/common.sh` |
| `scripts/build-images.sh` | `images/build.sh` |
| `scripts/build-packages.sh` | `packaging/build-packages.sh` |
| `scripts/showcase/`, `scripts/update-website-screenshots.sh` | `docs/screenshots/` |
| `packaging/` | stays: already one purpose. Its runtime files (`patch-makechrootpkg.py`, `sandbox-protected`, the units) are what packages and images both install |
| `docs/` | stays, plus `docs/screenshots/` |

`scripts/` disappears. `tools/` holds only what several purposes share; a
script used by one purpose lives in that purpose's tree. The `Justfile` is
the stable way in: `just test-e2e`, `just test-kernel`,
`just test-isolation`, `just test-browser`, `just up`, `just images`,
`just packages`. CI calls the scripts directly.

### Moving what users fetch by URL

A rename breaks a URL: neither `raw.githubusercontent.com` nor GitHub's
`blob/` and `tree/` links follow one, and a symlink at the old path serves
the link's text, not the file. What is published where:

| Path | Published through | In a release |
|---|---|---|
| `compose/docker-compose.yaml` | `curl -O` of its raw URL in `README.md`, `docs/docs/overview/quick-start.md`, `docs/docs/setup/docker.md`, `compose/README.md`; read by a test in `aurcli`'s `compose.rs` | 0.6.0 |
| `compose/docker-compose.remote-worker.yaml` | `blob/master` links in `docs/docs/setup/docker.md`, `setup/kubernetes.md`, `examples/raspberry-pi.md`; `README.md`; mirrored by `aurcli setup compose` | 0.6.0 |
| `compose/docker-compose.demo.yaml` | `docs/docs/workers/demo.md` (a checkout path, no URL) | 0.6.0 |
| every file in `compose/` | `docs/docs/setup/artifacts/docker-images.md:29-35`: a table naming each file "under `compose/`", the e2e ones as `docker-compose.e2e*.yaml` | 0.6.0 |
| `kubernetes/**` | `docs/docs/setup/kubernetes.md`: `kubectl apply -k kubernetes/`, a `tree/master` link; `README.md` links the directory | not yet |

The docs site is built from `master` and is not versioned, and `aurcli`
renders its compose templates from compiled-in text, not from these files,
so everything in the repository can follow a move in the same PR. What
cannot follow is a copy of an old instruction: a third-party guide, a
user's notes, a script that fetches the raw URL. For those, `curl -O` of a
missing file writes GitHub's `404: Not Found` into `docker-compose.yaml`,
and `docker compose` fails on it with a YAML error that says nothing about a
move.

**Recommended: keep a copy for one release.** The PR that moves the compose
files:

- moves them to `deploy/compose/`, and updates `README.md`, the docs,
  `AGENTS.md`, the `aurcli` comments and its comparison test in the same
  commit;
- leaves `compose/docker-compose.yaml` and
  `compose/docker-compose.remote-worker.yaml` behind as copies whose first
  line says where the file lives now and in which release the copy goes. A
  test in `aurcli` beside the comparison test asserts each copy equals its
  original after that line, so the two cannot drift;
- leaves `compose/README.md` as a pointer, and no copy of the demo file,
  which nothing outside the repository reads by path;
- adds a release-notes line: the new paths, and that the old ones go in the
  next release.

The release after the one that ships the move deletes `compose/`.
Updating everything in-repo without a copy breaks the out-of-tree
instructions with that unhelpful error; a stub instead of a copy breaks
them with a better one, but a compose file cannot redirect, so it breaks
them all the same. The copy costs two files and one test for one release.

`kubernetes/` is in no release yet: it was added on 2026-11-16, after 0.6.0.
It moves to `deploy/kubernetes/` with no copy, in a PR of its own as soon as
#438 and #447 land (step 1), which nothing else waits on. If a release is cut
before that PR lands, `kubernetes/` ships at its old path and then moves
under the compose files' rule instead: in step 5, with a copy left at
`kubernetes/` for one release, a CI check that `diff -r` finds the two trees
equal apart from a first-line notice in `kustomization.yaml`, and the same
release-notes line. The copy is whole rather than a stub kustomization over
the new path, because users edit `kubernetes/kustomization.yaml` itself to
choose their worker components.

## The build runner

`build-runner.md` replaces the patched `makechrootpkg` with AURCache's own
runner; from its step 2 the PKGBUILD no longer runs on the host at all. The
`build-isolation` suite is written against the worker's entry point
(`aurcache-build --interface=N`, whatever is behind it) and against
outcomes: the hostile fixture's attempts fail, and an unconfined control lets
the same fixture succeed. It asserts nothing about Landlock or the patch. When
the runner lands, the confined leg keeps passing through the same entry
point; the Landlock-specific expectations (#450's `RESOLVE_UNIX`) become
"a build not offered the agent cannot reach it", whatever enforces it.

The unconfined control is stock `makechrootpkg` from devtools. It only has
to prove the fixture is hostile, so it does not need to be AURCache's code,
and it keeps working after the patch is gone. If devtools stops shipping
it, the control becomes a test-only unconfined mode of the runner.

## Migration

Step 1 waits only for #438 and #447. The others wait for #435, #446, #450
and #452 as well, which edit the scripts, compose files and fixtures that
move. Each step is one PR and leaves every suite working, not only the CI
jobs: the e2e scripts no workflow runs (`test-e2e-hybrid.sh`,
`test-e2e-hybrid-legacy.sh`, `test-e2e-ssh.sh`) and the screenshot tooling
keep running from their own paths until the step that replaces them. An old
file is deleted in the step that moves its last user, never earlier.

1. **`deploy/kubernetes/`, as soon as #438 and #447 land**, before the next
   release (see [above](#moving-what-users-fetch-by-url) if one comes
   first). `kubernetes/` to `deploy/kubernetes/`, no copy. Follow it: the
   manifests' own comments (`kubectl apply -k kubernetes/`),
   `kubernetes/README.md` and `kube-worker/README.md`, `README.md`'s link,
   `docs/docs/setup/kubernetes.md` and `docs/docs/workers/kubernetes.md`;
   #447's `scripts/fixtures/kube-e2e/kustomization.yaml`, whose
   `../../../kubernetes/server` and `../../../kubernetes/kube-worker` become
   `../../../deploy/kubernetes/...`, and `test-e2e-kube.sh`, which applies
   `kubernetes/kube-worker/networkpolicy.yaml` by path. `AGENTS.md`: the
   `test-e2e-kube.sh` bullet of "Which end-to-end suite to run" names
   `kubernetes/kube-worker/`. `compose/README.md` names nothing there.
   Verifies: `kubectl kustomize` renders identical output from each
   directory users apply (`.`, `server/`, `chroot-worker/`,
   `chroot-worker-arm64/`, `kube-worker/`) at both paths; one
   `run-e2e-kube` run passes; the docs build; `rg` finds no path to
   `kubernetes/` outside `deploy/`.
2. **Fixtures**, paths only. `docker/testpkg/` to
   `tests/fixtures/packages/`; `docker/git-ssh.Dockerfile` and its
   entrypoint to `tests/fixtures/images/gitssh/`; `scripts/fixtures/` split
   three ways, as in the table above: `frontend-seed.sql` to
   `tests/browser/seed.sql`, `build-agent/` to `tests/build-isolation/agent/`,
   `kube-e2e/` to `tests/e2e/topologies/kube/` (its resources become
   `../../../../deploy/kubernetes/...`). The scripts and compose `build:`
   blocks naming them follow. `AGENTS.md` and `compose/README.md` name none
   of these files. Verifies: the per-push jobs pass; one `run-e2e` and one
   `run-e2e-kube` run pass; `test-e2e-ssh.sh` passes locally, building the
   git server from its new path; `rg` finds no old path.
3. **Component suites and the shared library.** `test-kernel.sh` and
   `test-frontend.sh` to `tests/{kernel,browser}/run.sh`; `test-sandbox.sh`
   and `test-build-agent.sh` to `tests/build-isolation/{chroot,agent}/run.sh`,
   with `tests/build-isolation/run.sh` running both; `scripts/lib/common.sh`
   to `tools/lib/`, for `build-images.sh`, `build-packages.sh` and the
   browser suite; `just` targets; `ci.yml`. `AGENTS.md`: the command block's
   `just test-browser`, `test-kernel` and `test-agent` comments and its
   `./scripts/test-frontend.sh` and `./scripts/test-build-agent.sh` lines;
   the `test-build-agent.sh`, `test-sandbox.sh` and `test-kernel.sh` bullets
   of "Which end-to-end suite to run"; the paragraph on what CI runs.
   Verifies: the same CI jobs pass with the same test counts;
   `build-images.sh` and `build-packages.sh` still print their help.
4. **CI compilation** (D10, D11). The Cargo cache in each compiling job;
   the kernel tests in the Rust job from its build. `AGENTS.md`: the
   paragraph on what CI runs loses `test-kernel.sh` as a job of its own.
   Verifies: the same four kernel test binaries run; a second `master` run
   restores the cache and its Rust job is shorter; a PR run restores and
   saves nothing; the entries' total is within
   [the estimate](#size-against-the-quota), and a week on, the publish run's
   image builds still hit their cache.
5. **`deploy/compose/`.** The three user compose files to
   `deploy/compose/`, with copies of the turnkey and remote-worker files
   (none of the demo file), their test and the release-notes line as above.
   The demo file's two `context: ..` and the turnkey file's three commented
   ones become `../..`. `deploy/compose/README.md` takes the user-facing
   table, the demo row and the run-from-root paragraph, with
   `deploy/compose/` paths and `context: ../..`; `compose/README.md` keeps
   the development and e2e tables for the files still in `compose/` and
   says where the others went. Docs: `README.md`, `quick-start.md`,
   `setup/docker.md`, `setup/kubernetes.md`, `examples/raspberry-pi.md`,
   `workers/demo.md`, and `setup/artifacts/docker-images.md`, whose table
   now puts the user files under `deploy/compose/` and the e2e files, still,
   under `compose/`. `AGENTS.md`: the `aurcli setup` paragraph's
   `compose/docker-compose.yaml`; the bullet on containerized workflows,
   whose "all under `compose/`" stops being true. `aurcli`'s comments and
   comparison test. Verifies: `aurcli`'s comparison and copy tests pass;
   `docker compose -f deploy/compose/docker-compose.demo.yaml config`
   resolves both build contexts to the repository root; the docs build; the
   raw URL of the old path on the branch serves the copy.
6. **Harness and reference topology.** `tests/e2e/run.sh` and `up.sh`,
   `topologies/lib/compose.sh`, the `chroot` topology (an overlay over
   `deploy/compose/docker-compose.yaml`, with its local image tags), `basic`.
   `scripts/lib/e2e.sh` moves to `tests/e2e/lib/e2e.sh`: the new harness is
   built on it rather than on a copy, and `test-e2e.sh` and
   `test-e2e-ssh.sh` source it from there (the comment in
   `docker-compose.e2e.yaml` naming it follows). The nightly and `run-e2e` run
   `tests/e2e/run.sh --topology chroot basic`; `just test-e2e`. Kept, because
   other suites still use them: `scripts/test-e2e.sh`, which both hybrid
   wrappers `exec`; `compose/docker-compose.e2e.yaml`, which
   `test-e2e-ssh.sh` layers its overlay onto and `scripts/showcase/
   generate.sh` starts. `AGENTS.md`: the command block gains
   `just test-e2e`, and "Which end-to-end suite to run" names it as the
   default, leaving the hybrid and SSH bullets as they are.
   `compose/README.md`: the `docker-compose.e2e.yaml` row says which suites
   still use it, and that the nightly does not. Verifies: a `run-e2e` run
   builds and installs `hello` through the `repo` nginx service;
   `just up chroot` serves the UI; no `ghcr.io` image is tagged by the
   run; `test-e2e-hybrid.sh`, `test-e2e-hybrid-legacy.sh` and
   `test-e2e-ssh.sh` pass locally.
7. **Hybrid topologies** (D1, D2). Their compose files into
   `tests/e2e/topologies/hybrid{,-host}/`; the weekly schedule. Delete both
   wrappers, both `.dev.yaml` and `scripts/test-e2e.sh`, whose only script
   callers the wrappers were; the comments naming `test-e2e.sh` follow
   (`fake_worker.rs`, `ci.yml`, `generate.sh`'s header,
   `docker-compose.e2e.yaml`'s header). `AGENTS.md`: the
   `./scripts/test-e2e.sh hello` line becomes `just test-e2e`; the two
   hybrid bullets name `run.sh --topology hybrid` and `hybrid-host`; the
   containerized-workflows bullet loses the `.dev.yaml` files (now
   `just up hybrid{,-host}`) and `test-e2e.sh`. `compose/README.md`: the two
   `.dev.yaml` rows and the two hybrid e2e rows go, and the first e2e row
   stops naming `test-e2e.sh`. Verifies: `run-e2e:hybrid` and
   `run-e2e:hybrid-host` pass; `just up hybrid-host` builds through the
   host's Docker; `test-e2e-ssh.sh` still passes.
8. **SSH** (D4-D7). The `gitssh` fixture
   (`tests/e2e/fixtures/gitssh/`, running the image from step 2) and
   `ssh-credentials`; the API test for the clone refusal and the worker
   test for the not-listed log line. `scripts/showcase/generate.sh` builds
   its real package in the `chroot` topology (`tests/e2e/up.sh chroot
   --fresh`, then `down`). Delete `test-e2e-ssh.sh`,
   `docker-compose.e2e-ssh.yaml` and `docker-compose.e2e.yaml`, whose last
   users these were; `tests/e2e/lib/e2e.sh` drops what only they used.
   `AGENTS.md`: the containerized-workflows bullet stops naming
   `docker-compose.e2e.yaml`. `compose/README.md`: the e2e table goes,
   leaving the pointer to `deploy/compose/` beside step 5's copies.
   `docs/docs/setup/artifacts/docker-images.md`: the e2e row goes, or names
   `tests/e2e/topologies/`. Verifies: the scenario passes, and fails with
   #452's fix reverted; `generate.sh` builds its real package; `rg` finds no
   `docker-compose.e2e`.
9. **Kube topology** (D9, D12). The kind driver behind the topology hooks
   (`tests/e2e/topologies/kube/hooks.sh`); `test-e2e-kube.sh`'s legs become
   scenarios, three of them shared, and the packages the shared ones use
   join `tests/fixtures/packages/`; `gitssh.yaml` becomes the `gitssh`
   fixture's Kustomize component. One e2e workflow with the image job, the
   `run-e2e` and `run-e2e:<topology>` labels; `e2e-kube.yml`,
   `run-e2e-kube` and `test-e2e-kube.sh` go. `AGENTS.md`: the
   `./scripts/test-e2e-kube.sh` lines of the command block (`E2E_BIG`,
   `KEEP`, `REUSE`, `E2E_LEGS`) become their `run.sh --topology kube`
   equivalents; the kube bullet; the CI paragraph's workflows and labels.
   Verifies: every former leg reports pass or skip in the summary; the
   nightly takes no longer than today's two workflows in parallel.
10. **Hostile fixtures** (D8). One set; `build-isolation` runs it through
    both executors' entry points, `kube` through its pods. Verifies: each leg
    still fails with its executor's confinement disabled.
11. **`images/` and the rest of `scripts/`.** `docker/` to `images/`,
    `build-images.sh` to `images/build.sh`, `build-packages.sh` to
    `packaging/`, the screenshot tooling to `docs/screenshots/`; the publish
    workflows, compose `build:` blocks (the `dockerfile:` lines, in
    `deploy/compose/`, the topologies and the copies alike), `.dockerignore`
    and docs follow. `AGENTS.md`: the submodule section's
    `scripts/build-packages.sh`, the command block's `build-packages.sh` and
    `build-images.sh` lines. `scripts/` is gone. Verifies:
    `images/build.sh` builds every image for one platform;
    `docs/screenshots/update.sh` produces the same pictures.
12. **In the release after the one that ships step 5:** delete `compose/`
    (and `kubernetes/`, if it moved under the fallback) and the copy test.

Then `chroot-2`, with the existing scenarios run on it so that whatever
assumes a single worker is fixed before new scenarios copy it; then the
scenarios listed in `e2e-scenarios.md`, one file each.
