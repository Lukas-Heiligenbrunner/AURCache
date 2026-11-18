# Test architecture and repository layout

Which tier a test belongs to, how the end-to-end suites share one harness
instead of one script per concern, and a repository organised by purpose:
what users deploy, what the product is built from, and what tests it each
have their own tree, subdivided by kind only inside it.

`e2e-scenarios.md` defines the harness's contracts (topology, scenario,
fixture, helpers) and stays the reference for them. This design adds the
tiers around that harness, the Kubernetes topology, a deliberate CI matrix,
the review of duplicated coverage and the repository layout. It replaces that
design's Layout and Migration sections and answers its question of where the
e2e compose files live.

Status: **Proposed** · Last updated: 2026-11-18

---

## Decided

- **Purpose first.** Top-level directories are named for a purpose (deploy,
  images, packaging, tests, docs), and subdivided by kind inside one where
  useful. See [Layout](#layout).
- **`tests/` at the repository root**, holding every test that is not
  `cargo test`.
- **The `.dev.yaml` compose files go.** The hybrid topologies' files replace
  them, and `just up <topology>` starts any topology for development. See
  [Starting a topology for development](#starting-a-topology-for-development).
- **The reference topology is the user compose file plus a test overlay.**
- **CI uses a Cargo cache**, with `Swatinem/rust-cache`, saved only from
  `master`. See [CI compilation](#ci-compilation).
- **One `run-e2e` label** runs the nightly matrix on a pull request, and
  **`run-e2e:<topology>`** one column. `run-e2e-kube` goes.
- **The hybrid topologies run weekly**, not nightly.
- **The isolation suite's unconfined control is stock `makechrootpkg`** for
  as long as devtools ships it.

## Inventory

Runtimes are CI wall-clock from the master runs of 2026-11-18 unless marked
local. "Open PR" rows describe the PR's head as of the date above.

### Every push (`ci.yml`)

| Job / suite | Covers | Runtime | Uses |
|---|---|---|---|
| `lintAndTestRust`: `cargo test --all`, `cargo test -p aurcache-api --features static` | Unit and integration tests of the backend workspace, including the hermetic fake worker (`aurcache-worker/tests/fake_worker.rs`), the worker-protocol fixtures and PKGBUILD parsing under the sandbox | 14 min job, of which 7 min test step; the tests themselves ~8 s locally | `aurcache-common/tests/fixtures/worker-protocol/`; the `alpm-pkgbuild-bridge` script downloaded from GitLab |
| same job: "Test PKGBUILD parsing as root" | The parse tests again as root, with and without Landlock, as the images and a Landlock-less kernel run them | 1 min | the test binary built above |
| same job: package build shapes, driver-free types | `cargo check` of each exact package and image invocation | in the 14 min | -- |
| `lintAndTestFrontend` | fmt, clippy for wasm and host, `cargo test` | 5 min | -- |
| `browserTest`: `scripts/test-frontend.sh` | Every route mounts in Chrome against a seeded server with the UI embedded | 8 min (~1 min locally, warm) | `scripts/fixtures/frontend-seed.sql` |
| `kernelTests`: `scripts/test-kernel.sh` | Storage pool (loop, btrfs simple quotas), pool drain, cache volumes, cgroup peak, as root in a privileged container | 13.5 min, nearly all of it compiling test binaries | `archlinux/archlinux` image |
| `sandboxTest`: `scripts/test-sandbox.sh` | The Landlock confinement around a PKGBUILD sourced outside the chroot, in both polarities: denied through `aurcache-build`, escaping through stock `makechrootpkg` | 3 min | Dockerfile generated inline; `docker/nspawn-wrapper.sh`, `packaging/patch-makechrootpkg.py`, `packaging/sandbox-protected`, `docker/testpkg/hostile-fixture/` |

No job caches Cargo output: each compiles its part of the workspace from
scratch.

### Nightly, when something landed in 25 h, or on the `run-e2e` label

| Suite | Covers | Runtime | Uses |
|---|---|---|---|
| `scripts/test-e2e.sh hello` | Server and one chroot worker from this tree: enrollment, a real AUR build, disk usage reported, `pacman -S` from the repository | 22-29 min in CI (image builds dominate); ~4 min locally with warm images | `compose/docker-compose.e2e.yaml`, `docker/server.Dockerfile`, `docker/arch.Dockerfile` (`worker`), `scripts/lib/e2e.sh`, the AUR and Arch mirrors |

### In no workflow

| Suite | Covers | Uses |
|---|---|---|
| `scripts/test-e2e-hybrid.sh` | The hybrid image in DinD mode: embedded worker enrolls over loopback, Podman inside the container, the Docker worker builds | `compose/docker-compose.e2e-hybrid.yaml`, `docker/arch.Dockerfile` (`hybrid`) |
| `scripts/test-e2e-hybrid-legacy.sh` | The same image with the host's Docker socket: the build directory shared with an outside daemon | `compose/docker-compose.e2e-hybrid-legacy.yaml`, `docker/builder.Dockerfile`, `/tmp/aurcache-legacy-builds` on the host |
| `scripts/test-e2e-ssh.sh` | Build credentials reach the build user inside the chroot: unauthorised fails, authorised succeeds with the payload's marker installed | `compose/docker-compose.e2e.yaml` + `docker-compose.e2e-ssh.yaml`, `docker/git-ssh.Dockerfile`, `docker/git-ssh-entrypoint.sh`, `docker/testpkg/ssh-fixture/` |
| `scripts/update-website-screenshots.sh`, `scripts/showcase/` | Not tests: website screenshots from a restored snapshot | `scripts/showcase/snapshot/` |
| `conformance-tests.md` | Proposed, not built: black-box checks in each PKGBUILD's `check()` | -- |

### Open PRs

| PR | Test additions |
|---|---|
| #435 | `aurcache-utils/tests/ssh_clone.rs` (server clones over SSH from a throwaway `sshd`, per push); a third `test-e2e-ssh.sh` phase for the same server-side clone through the API; `docker/testpkg/ssh-source-fixture/`; changes to `git-ssh.Dockerfile` and its entrypoint |
| #446 | `scripts/test-build-agent.sh`, a new per-push job (~3 min): the static build agent in the builder image under a build container's flags; legs: `hello` builds, a hostile build (root, the SSH key, the pacman helper), stop by SIGKILL/SIGTERM, arm64 under qemu where registered. `docker/testpkg/agent-hostile/`, `docker/testpkg/agent-thief/`, `scripts/fixtures/build-agent/key-scan.pl`, `git-ssh.Dockerfile` reused. Hybrid-legacy now runs on the agent. |
| #447 | `scripts/test-e2e-kube.sh` (1375 lines, ~20 min locally, ~10 GB) and `e2e-kube.yml`, nightly and on a `run-e2e-kube` label. Eleven legs: enrollment, NetworkPolicy, ResourceQuota, a dependency only in AURCache's repository, SSH through the agent with a thief dependency, cancel, worker killed, worker pod deleted, pod that cannot start, a 2 GiB package, user namespaces, policy removed. `scripts/fixtures/kube-e2e/` (overlays, gitssh, five packages), `docker/testpkg/big/`. Deploys `kubernetes/server/` and `kubernetes/kube-worker/` (#438) as shipped. |
| #450 | A `test-sandbox.sh` leg: a build not offered the worker's ssh-agent is refused its socket (Landlock `RESOLVE_UNIX`, Linux 7.1). A third `test-e2e-ssh.sh` phase: a package not in `WORKER_GIT_SSH_PACKAGES` gets no key. Unit tests for the key request, the offer and the package ceiling. |
| #451 | A check in `test-sandbox.sh` for the system upgrade without AURCache's repository. Its own e2e script is being replaced by that check and is not counted here. |
| #452 | Two `test-e2e-ssh.sh` phases: `WORKER_SSH_KNOWN_HOSTS` naming the server builds, naming another host is refused. Unit tests for staging and binding `known_hosts`. |

With #435, #450 and #452 merged, `test-e2e-ssh.sh` has six phases; the two
on master each start from `down -v`, so a fresh worker volume and base
chroot.

### Where test files are

Test-only files sit in five places: `docker/testpkg/` (eight package
fixtures once the PRs land), `docker/git-ssh*` (and `git-fixtures.Dockerfile`
in #451), `compose/docker-compose.e2e*.yaml` (four, five with #451),
`scripts/fixtures/` (the frontend seed, `build-agent/`, `kube-e2e/`), and
Dockerfiles written inline by `test-sandbox.sh`. Hostile packages exist three
times (`hostile-fixture`, `agent-hostile`/`agent-thief`,
`kube-e2e-ssh`/`kube-e2e-thief`), each with its own idea of what to try.

## Duplicated and redundant work

| # | Finding | Recommendation | Saving |
|---|---|---|---|
| D1 | `compose/docker-compose.{hostmode,dindmode}.dev.yaml` are the two hybrid e2e files with fixed ports, `LOG_LEVEL=debug` and bind-mounted data. | **Drop** them; the topology files are the only copy, started with `just up hybrid` or `just up hybrid-host`. | Two files that drift |
| D2 | `test-e2e-hybrid.sh` and `-hybrid-legacy.sh` run the same image, the same Docker worker and the same scenario; only the daemon's location differs. Neither runs in CI. | **Keep both as topologies** (each is a distinct upgrade path), **drop both scripts**. Run `basic` on each weekly; one run builds the hybrid image once for both. | One image build (cached layers aside) and two wrapper scripts; coverage goes from none to weekly |
| D3 | `docker-compose.e2e.yaml` is a second copy of the turnkey stack, already different from it: the repository served by the server rather than the `repo` nginx service, `WORKER_CONCURRENCY` rather than `WORKER_CONCURRENCY_DEFAULT`, another network name. The nginx service users run is tested nowhere. | **Merge**: the reference topology is `deploy/compose/docker-compose.yaml` plus a test overlay (build from the tree, ports, the env hooks), as #447 deploys the shipped manifests plus overlays. | One file, and coverage of what users deploy |
| D4 | #435's server-clone phase of `test-e2e-ssh.sh` repeats `ssh_clone.rs`, which clones with a real `sshd` per push. What the phase adds is the refusal message through the API. | **Drop** the phase; assert the message in an API test. | One stack cycle (estimate 3-5 min with a fresh worker) |
| D5 | #450's "not listed" phase repeats the ceiling's unit tests (`unset_is_every_package`, `empty_lists_no_package`, `membership_is_exact`); its enforcement is the sandbox leg's. | **Drop** the phase; assert the log line in the worker's unit test. | One stack cycle |
| D6 | #452's "unknown host" phase tests OpenSSH's host-key check once the file is used; `git_ssh_command_uses_the_agent_and_sets_host_policy` and the staging tests show it is used. The "pinned host" phase is the one that catches the readability bug the PR fixes. | **Keep** pinned host, folded into `ssh-credentials` (below). **Drop** unknown host. | One stack cycle |
| D7 | The remaining SSH phases recreate the whole stack between phases. | **Merge** into one `ssh-credentials` scenario (unauthorised, then authorised with a pinned `known_hosts`), recreating workers only (`e2e-worker-env`), as `e2e-scenarios.md` specifies. | Six fresh stacks become one stack and one worker recreate |
| D8 | Three hostile fixture sets (`hostile-fixture`, `agent-hostile`+`agent-thief`, `kube-e2e-ssh`+`kube-e2e-thief`) each encode what a hostile build tries. `key-scan.pl` exists only for the build agent. | **Merge** into one fixture set under `tests/fixtures/packages/hostile*` with one list of attempts; every executor's suite runs the same set. Keep the per-executor legs: the key sits in a different place for each. | Fixture drift; the next executor gets the attempts for free |
| D9 | Kube's dependency-from-repository, cancel and worker-restart legs are meaningful on every topology, and the chroot worker has no e2e for any of them. | **Merge** into shared scenarios (`repo-dependency`, `cancel`, `worker-restart`) run on the reference topology and on `kube`. Keep the kube-only legs (policy, quota, stop, stuck pod, 2 GiB relay, user namespaces, open policy) as scenarios requiring kube capabilities. | No time; three scenarios written once instead of per executor |
| D10 | `kernelTests` compiles the four test binaries that `lintAndTestRust` compiles too; the tests run in seconds. | **Merge**: run them in the Rust job from that job's build. `test-kernel.sh` takes the executables from `cargo test --workspace --no-run` there rather than `-p`, whose different feature set would recompile. | ~12 runner-minutes per push; the Rust job grows ~1-2 min (pacman in the container plus the tests) |
| D11 | Every compiling job starts from an empty `target/`. | **Cache** Cargo output per job ([CI compilation](#ci-compilation)). | Most of each job's compile time on a warm cache (estimate: the Rust job's 14 min to 5-7) |
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

A new script is never the answer. A fix whose test needs a new script
names the missing helper, capability or leg instead.

## End to end: topologies × scenarios

### Topologies

| Topology | Stack | Workers | Capabilities | Built from |
|---|---|---|---|---|
| `chroot` (reference) | compose | 1 chroot worker | `chroot disk-usage kept-tree` | `deploy/compose/docker-compose.yaml` + overlay |
| `chroot-2` | compose | 2 chroot workers | + `multi-worker` | same, second worker in the overlay |
| `hybrid` | compose | Docker worker in the server container, Podman inside | `container disk-usage` | own file (shaped like a pre-worker deployment on purpose) |
| `hybrid-host` | compose | same, host's Docker socket | `container disk-usage` | own file; `setup.sh` builds the builder image |
| `kube` | kind | Kubernetes worker, build pods | `container disk-usage network-policy quota pod-lifecycle` | `deploy/kubernetes/server/` + `deploy/kubernetes/kube-worker/` + overlays |

The `chroot` overlay replaces the user file's `image:` lines with `build:`
from this tree, and adds the ports and the env hooks the topology contract
requires. Built from the user file, the repository is the `repo` nginx
service on that file's network, not `http://aurcache:8081` on
`aurcache_network`; the topology declares both (`E2E_REPO_URL`,
`E2E_NETWORK`), and `e2e-install` and the fixtures read them.

`kube` needs the topology contract to stop assuming compose. A topology
directory provides `up`, `down`, `logs <worker>`, `worker-env` and
`worker-kill <worker>`; `tests/e2e/topologies/lib/compose.sh` implements
them once for the compose topologies, and `kube/` implements them with
`kind`, `kubectl` and kustomize. Scenarios see only `aurcli`, `jq` and the
`e2e-*` helpers, as before; `e2e-worker-kill` is the new helper, which the
worker-restart and crash scenarios need.

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
`hybrid-host`. For the nine scenarios in `e2e-scenarios.md`, the shared
`repo-dependency` and `cancel`, and the seven kube-only legs, the full
product over five topologies is 90 runs; this matrix is about 27 across the
nightly and weekly runs, and adding a shared scenario adds one run, not five.

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
  small.
- **Root-written files out first.** The kernel tests, once they run in the
  Rust job (D10), write pool images as root under `CARGO_TARGET_TMPDIR`; the
  step removes that directory before the cache's post step archives
  `target/`.

`actions/cache` keyed by hand could do the same, with the save gating, the
pruning and the key written out in every job; the action is those defaults.
The e2e image job uses buildx's `gha` cache, already used by
`build-images.sh --cache gha` in the publish workflow, with one scope per
image.

## Layout

### Purpose first

Each top-level directory answers one question; inside it, files are grouped
by kind where that helps.

```
backend/  frontend-rs/  assets/   the product's source
deploy/                           running AURCache
  compose/                        docker-compose.yaml, docker-compose.remote-worker.yaml,
                                  docker-compose.demo.yaml, README.md
  kubernetes/                     what kubernetes/ holds today
images/                           the product's container images (today's docker/)
  server.Dockerfile  arch.Dockerfile  builder.Dockerfile  demo-worker.Dockerfile
  entrypoints, build-time scripts, podman and pacman config
  build.sh                        today's scripts/build-images.sh
packaging/                        the Arch packages: AUR submodules, units, install-files.sh,
                                  patch-makechrootpkg.py, sandbox-protected, ...
  build-packages.sh               today's scripts/build-packages.sh
tests/                            every test that is not `cargo test`
  README.md                       the tiers, the rule, how to run each
  fixtures/
    packages/                     hello, hostile*, ssh, big, replaces-*, ...
    gitssh/                       Dockerfile, entrypoint.sh
    frontend-seed.sql
  kernel/run.sh
  build-isolation/                run.sh, chroot/ (probe, image), agent/ (key-scan.pl)
  browser/run.sh
  e2e/
    run.sh  up.sh  lib/  bin/
    topologies/{chroot,chroot-2,hybrid,hybrid-host,kube}/   compose or kustomize overlays
    fixtures/{gitssh,...}/
    scenarios/*.sh
docs/                             the website
  screenshots/                    update.sh (today's update-website-screenshots.sh), showcase/
design/                           unchanged
tools/lib/common.sh               shell helpers tests, images and packaging all source
```

Where today's contents go:

| Today | Goes to |
|---|---|
| `compose/docker-compose{,.remote-worker,.demo}.yaml`, `compose/README.md` | `deploy/compose/` |
| `compose/docker-compose.e2e*.yaml` | `tests/e2e/topologies/*/`, `tests/e2e/fixtures/*/` |
| `compose/docker-compose.*.dev.yaml` | removed (D1) |
| `kubernetes/` | `deploy/kubernetes/` |
| `docker/*.Dockerfile`, entrypoints, image build scripts, configs | `images/` |
| `docker/testpkg/`, `docker/git-ssh*` | `tests/fixtures/` |
| `scripts/test-*.sh` | `tests/<suite>/run.sh` |
| `scripts/lib/e2e.sh` | `tests/e2e/lib/` |
| `scripts/lib/common.sh` | `tools/lib/common.sh` |
| `scripts/fixtures/` | `tests/fixtures/`, `tests/build-isolation/agent/`, `tests/e2e/topologies/kube/` |
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
| `kubernetes/**` | `docs/docs/setup/kubernetes.md`: `kubectl apply -k kubernetes/server/`, a `tree/master` link | not yet |

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
  which is published by no URL;
- adds a release-notes line: the new paths, and that the old ones go in the
  next release.

The release after the one that ships the move deletes `compose/`.
Updating everything in-repo without a copy breaks the out-of-tree
instructions with that unhelpful error; a stub instead of a copy breaks
them with a better one, but a compose file cannot redirect, so it breaks
them all the same. The copy costs two files and one test for one release.

`kubernetes/` is in no release yet. It moves to `deploy/kubernetes/` with
no copy, provided the move lands before the next release.

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

After #435, #438, #446, #447, #450, #451 and #452 land: they edit the
scripts, compose files, manifests and fixtures that move. Each step is one
PR and keeps CI green alone. Step 4 has to land before the next release, so
that `kubernetes/` never ships at its old path.

1. **Fixtures.** `docker/testpkg/`, `docker/git-ssh*` (and #451's
   `git-fixtures.Dockerfile`) and `scripts/fixtures/` to `tests/fixtures/`.
   Paths only. Verifies: per-push jobs pass; one `run-e2e` and one
   `run-e2e-kube` run pass; `rg` finds no old path.
2. **Component suites and the shared library.** `test-kernel.sh`,
   `test-frontend.sh`, `test-sandbox.sh`, `test-build-agent.sh` to
   `tests/*/run.sh`; `scripts/lib/common.sh` to `tools/lib/`; `just`
   targets; `AGENTS.md`. Verifies: the same CI jobs pass with the same test
   counts; `build-images.sh` and `build-packages.sh` still print their help.
3. **CI compilation** (D10, D11). The Cargo cache in each compiling job;
   the kernel tests in the Rust job from its build. Verifies: the same four
   kernel test binaries run; a second `master` run restores the cache and
   its Rust job is shorter; a PR run restores and saves nothing.
4. **`deploy/`.** `kubernetes/` to `deploy/kubernetes/` (no copy); the
   three user compose files to `deploy/compose/`, with the two copies, their
   test and the release-notes line as above; docs, `README.md`, `AGENTS.md`,
   `aurcli`'s comments and test. Verifies: `kubectl kustomize` renders the
   same manifests from the new paths as from the old (`diff` of the output);
   `aurcli`'s comparison and copy tests pass; the docs build; the raw URL of
   the old path on the branch serves the copy.
5. **Harness and reference topology.** `tests/e2e/run.sh` and `up.sh`, the
   `chroot` topology from `deploy/compose/docker-compose.yaml` plus overlay,
   `basic`. The nightly calls it. Delete `scripts/test-e2e.sh` and
   `docker-compose.e2e.yaml`. Verifies: a `run-e2e` run builds and installs
   `hello` through the `repo` nginx service; `just up chroot` serves the UI.
6. **Hybrid topologies** (D1, D2). Their compose files into
   `tests/e2e/topologies/hybrid{,-host}/`; delete both wrappers and both
   `.dev.yaml`; the weekly schedule. Verifies: `run-e2e:hybrid` and
   `run-e2e:hybrid-host` pass; `just up hybrid-host` builds through the
   host's Docker.
7. **SSH** (D4-D7). The `gitssh` fixture overlay and `ssh-credentials`;
   the API test for the clone refusal and the worker test for the
   not-listed log line. Delete `test-e2e-ssh.sh` and its overlay.
   Verifies: the scenario passes, and fails with #452's fix reverted.
8. **Kube topology** (D9, D12). The kind driver behind the topology hooks;
   `test-e2e-kube.sh`'s legs become scenarios, three of them shared. One e2e
   workflow with the image job, the `run-e2e` and `run-e2e:<topology>`
   labels; `e2e-kube.yml` and `run-e2e-kube` go. Verifies: every former leg
   reports pass or skip in the summary; the nightly takes no longer than
   today's two workflows in parallel.
9. **Hostile fixtures** (D8). One set; `build-isolation` runs it through
   both executors' entry points, `kube` through its pods. Verifies: each leg
   still fails with its executor's confinement disabled.
10. **`images/` and the rest of `scripts/`.** `docker/` to `images/`,
    `build-images.sh` to `images/build.sh`, `build-packages.sh` to
    `packaging/`, the screenshot tooling to `docs/screenshots/`; the publish
    workflows, compose `build:` blocks, `.dockerignore` and docs follow.
    `scripts/` is gone. Verifies: `images/build.sh` builds every image for
    one platform; `docs/screenshots/update.sh` produces the same pictures.
11. **In the release after the one that ships step 4:** delete `compose/`
    and the copy test.

Then the scenarios listed in `e2e-scenarios.md`, one file each.

## Open questions

- **The demo instance.** Does anything outside the repository read
  `compose/docker-compose.demo.yaml` by path (the deployed demo's own
  setup)? If so it moves to `deploy/compose/` in step 4 too, or gets a copy
  like the other two.
