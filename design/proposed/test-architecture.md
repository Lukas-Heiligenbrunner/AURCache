# Test architecture and repository layout

Which tier a test belongs to, how the end-to-end suites share one harness
instead of one script per concern, and where test-only files live so that
`compose/`, `docker/` and `kubernetes/` hold only what users deploy.

`e2e-scenarios.md` defines the harness's contracts (topology, scenario,
fixture, helpers) and stays the reference for them. This design adds the
tiers around that harness, the Kubernetes topology, a deliberate CI matrix,
the review of duplicated coverage and the repository layout. It replaces that
design's Layout and Migration sections and answers its question of where the
e2e compose files live.

Status: **Proposed** · Last updated: 2026-11-18

---

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

### Nightly, when something landed in 25 h, or on the `run-e2e` label

| Suite | Covers | Runtime | Uses |
|---|---|---|---|
| `scripts/test-e2e.sh hello` | Server and one chroot worker from this tree: enrollment, a real AUR build, disk usage reported, `pacman -S` from the repository | 22-29 min in CI (image builds dominate); ~4 min locally with warm images | `compose/docker-compose.e2e.yaml`, `docker/server.Dockerfile`, `docker/arch.Dockerfile` (`worker`), `scripts/lib/e2e.sh`, the AUR and Arch mirrors |

### In no workflow

| Suite | Covers | Uses |
|---|---|---|
| `scripts/test-e2e-hybrid.sh` | The hybrid image in DinD mode: embedded worker enrolls over loopback, Podman inside the container, the Docker worker builds | `compose/docker-compose.e2e-hybrid.yaml`, `docker/arch.Dockerfile` (`hybrid`) |
| `scripts/test-e2e-hybrid-legacy.sh` | The same image with the host's Docker socket: the build directory shared with an outside daemon | `compose/docker-compose.e2e-hybrid-legacy.yaml`, `docker/builder.Dockerfile`, `/tmp/aurcache-legacy-builds` on the host |
| `scripts/test-e2e-ssh.sh` | Build credentials reach the build user inside the chroot: unauthorised fails, authorised succeeds with the payload's marker installed. Each phase starts from `down -v` | `compose/docker-compose.e2e.yaml` + `docker-compose.e2e-ssh.yaml`, `docker/git-ssh.Dockerfile`, `docker/git-ssh-entrypoint.sh`, `docker/testpkg/ssh-fixture/` |
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
| D1 | `compose/docker-compose.{hostmode,dindmode}.dev.yaml` are the two hybrid e2e files with fixed ports, `LOG_LEVEL=debug` and bind-mounted data. | **Merge**: the topology's compose file is the only copy; a developer runs `tests/e2e/run.sh --topology hybrid --keep`, or the file directly. Drop the two `.dev.yaml`. | Two files that drift |
| D2 | `test-e2e-hybrid.sh` and `-hybrid-legacy.sh` run the same image, the same Docker worker and the same scenario; only the daemon's location differs. Neither runs in CI. | **Keep both as topologies** (each is a distinct upgrade path), **drop both scripts**. Run `basic` on each nightly; one run builds the hybrid image once for both. | One image build (cached layers aside) and two wrapper scripts; coverage goes from none to nightly |
| D3 | `docker-compose.e2e.yaml` is a second copy of the turnkey stack, already different from it: the repository served by the server rather than the `repo` nginx service, `WORKER_CONCURRENCY` rather than `WORKER_CONCURRENCY_DEFAULT`, another network name. The nginx service users run is tested nowhere. | **Merge**: the reference topology is `compose/docker-compose.yaml` plus a test overlay (build from the tree, ports, the env hooks), as #447 deploys the shipped manifests plus overlays. | One file, and coverage of what users deploy |
| D4 | #435's server-clone phase of `test-e2e-ssh.sh` repeats `ssh_clone.rs`, which clones with a real `sshd` per push. What the phase adds is the refusal message through the API. | **Drop** the phase; assert the message in an API test. | One stack cycle (estimate 3-5 min with a fresh worker) |
| D5 | #450's "not listed" phase repeats the ceiling's unit tests (`unset_is_every_package`, `empty_lists_no_package`, `membership_is_exact`); its enforcement is the sandbox leg's. | **Drop** the phase; assert the log line in the worker's unit test. | One stack cycle |
| D6 | #452's "unknown host" phase tests OpenSSH's host-key check once the file is used; `git_ssh_command_uses_the_agent_and_sets_host_policy` and the staging tests show it is used. The "pinned host" phase is the one that catches the readability bug the PR fixes. | **Keep** pinned host, folded into `ssh-credentials` (below). **Drop** unknown host. | One stack cycle |
| D7 | The remaining SSH phases recreate the whole stack between phases. | **Merge** into one `ssh-credentials` scenario (unauthorised, then authorised with a pinned `known_hosts`), recreating workers only (`e2e-worker-env`), as `e2e-scenarios.md` specifies. | Six fresh stacks become one stack and one worker recreate |
| D8 | Three hostile fixture sets (`hostile-fixture`, `agent-hostile`+`agent-thief`, `kube-e2e-ssh`+`kube-e2e-thief`) each encode what a hostile build tries. `key-scan.pl` exists only for the build agent. | **Merge** into one fixture set under `tests/fixtures/packages/hostile*` with one list of attempts; every executor's suite runs the same set. Keep the per-executor legs: the key sits in a different place for each. | Fixture drift; the next executor gets the attempts for free |
| D9 | Kube's dependency-from-repository, cancel and worker-restart legs are meaningful on every topology, and the chroot worker has no e2e for any of them. | **Merge** into shared scenarios (`repo-dependency`, `cancel`, `worker-restart`) run on the reference topology and on `kube`. Keep the kube-only legs (policy, quota, stop, stuck pod, 2 GiB relay, user namespaces, open policy) as scenarios requiring kube capabilities. | No time; three scenarios written once instead of per executor |
| D10 | `kernelTests` compiles the four test binaries that `lintAndTestRust` compiles too; the tests run in seconds. | **Merge**: run them in the Rust job from that job's build. `test-kernel.sh` takes the executables from `cargo test --workspace --no-run` there rather than `-p`, whose different feature set would recompile. | ~12 runner-minutes per push; the Rust job grows ~1-2 min (pacman in the container plus the tests) |
| D11 | `browserTest` and `sandboxTest` each compile part of the workspace again (`static` server, release `aurcache-sandbox`). | **Keep** as separate jobs; share compilation through a Cargo cache if one is allowed (open question). | Up to ~6 runner-minutes per push with a cache (estimate) |
| D12 | With #447, the nightly builds the server image twice, in `e2e-nightly.yml` and `e2e-kube.yml`. | **Merge** into one nightly workflow: an image job builds and saves each image once, topology jobs load them. | One server image build per night (estimate 5-8 min of runner time) |
| D13 | `conformance-tests.md` proposes sandbox checks that "mirror `test-sandbox.sh`". | **Keep**, but run the sandbox suite's probe against the packaged binary instead of copying its cases. | A second copy avoided |
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
3. **End to end**: one harness, topologies × scenarios. Nightly, and on the
   `run-e2e` label.

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
| `chroot` (reference) | compose | 1 chroot worker | `chroot disk-usage kept-tree` | `compose/docker-compose.yaml` + overlay |
| `chroot-2` | compose | 2 chroot workers | + `multi-worker` | same, second worker in the overlay |
| `hybrid` | compose | Docker worker in the server container, Podman inside | `container disk-usage` | own file (shaped like a pre-worker deployment on purpose) |
| `hybrid-host` | compose | same, host's Docker socket | `container disk-usage` | own file; `setup.sh` builds the builder image |
| `kube` | kind | Kubernetes worker, build pods | `container disk-usage network-policy quota pod-lifecycle` | `kubernetes/server/` + `kubernetes/kube-worker/` + overlays |

Building `chroot` from the user file means the repository is no longer
always `http://aurcache:8081` on `aurcache_network`: there it is the `repo`
nginx service on the file's own network. The topology declares both
(`E2E_REPO_URL`, `E2E_NETWORK`) and `e2e-install` and the fixtures read
them.

`kube` needs the topology contract to stop assuming compose. A topology
directory provides `up`, `down`, `logs <worker>`, `worker-env` and
`worker-kill <worker>`; `topologies/lib/compose.sh` implements them once for
the compose topologies, and `kube/` implements them with `kind`, `kubectl`
and kustomize. Scenarios see only `aurcli`, `jq` and the `e2e-*` helpers, as
before; `e2e-worker-kill` is the new helper, which the worker-restart and
crash scenarios need.

The build runner's parity step (`build-runner.md`, step 1) is not a
topology: it is the `chroot` topology with the runner selected through
`e2e-worker-env`, and a scenario comparing the two packages.

### The CI matrix

Each scenario's header gains `ci:` next to `requires:`. `requires:` says
where it is meaningful; `ci:` says where CI runs it. CI runs:

- every scenario on the reference topology, `chroot`, unless its
  requirements exclude it;
- `basic` (`ci: all`) on every topology, as the smoke test;
- a scenario whose requirements only one topology meets (`network-policy`,
  `multi-worker`) on that topology;
- anything else only where `ci:` names it.

For the nine scenarios in `e2e-scenarios.md`, the shared `repo-dependency`
and `cancel`, and the seven kube-only legs, the full product over five
topologies is 90 runs; this matrix is about 27, and adding a shared scenario
adds one run, not five. One job per topology runs its cells in sequence with
one image load, the jobs in parallel, so a run takes as long as its slowest
column. `run-e2e` on a pull request runs the same matrix;
`run-e2e:<topology>` runs one column.

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

## Layout

```
compose/                     user-facing only
  docker-compose.yaml
  docker-compose.remote-worker.yaml
  docker-compose.demo.yaml
  README.md
kubernetes/                  unchanged: already user-facing only
docker/                      product images and the scripts they COPY
  server.Dockerfile  arch.Dockerfile  builder.Dockerfile  demo-worker.Dockerfile
  entrypoint.sh  worker-entrypoint.sh  hybrid-entrypoint.sh  ...
tests/
  README.md                  the tiers, the rule, how to run each
  fixtures/
    packages/                hello, hostile, hostile-thief, ssh, big, replaces-*, ...
    gitssh/                  Dockerfile, entrypoint.sh
    frontend-seed.sql
  kernel/run.sh
  build-isolation/           run.sh, chroot/ (probe, image), agent/ (key-scan.pl)
  browser/run.sh
  e2e/
    run.sh  lib/  bin/
    topologies/{chroot,chroot-2,hybrid,hybrid-host,kube}/
    fixtures/{gitssh,...}/   compose or kustomize overlays
    scenarios/*.sh
scripts/                     tools, not tests
  build-images.sh  build-packages.sh  update-website-screenshots.sh  showcase/
  lib/common.sh              shared by both; tests source it
```

Entry points are `tests/<suite>/run.sh` and the matching `just` target
(`just test-e2e`, `just test-kernel`, `just test-isolation`,
`just test-browser`). CI calls the scripts.

### What keeps its path

User-facing files are fetched by URL, and neither
`raw.githubusercontent.com` nor GitHub's `blob/` links follow a rename, so
there is no redirect to rely on. These stay where they are:

| File | Referenced by |
|---|---|
| `compose/docker-compose.yaml` | `curl -O` of its raw URL in `README.md`, `docs/docs/overview/quick-start.md`, `docs/docs/setup/docker.md`, `compose/README.md`; a test in `aurcache-cli/src/compose.rs` reads it to compare with `aurcli setup compose` |
| `compose/docker-compose.remote-worker.yaml` | `blob/master` links in `docs/docs/setup/docker.md`, `setup/kubernetes.md`, `examples/raspberry-pi.md`; `README.md`; mirrored by `aurcli setup compose` |
| `compose/docker-compose.demo.yaml` | `docs/docs/workers/demo.md`, and the deployed demo instance |
| `kubernetes/**` | `docs/docs/setup/kubernetes.md` (`kubectl apply -k kubernetes/server/`, a `tree/master` link); users' kustomizations may reference it remotely |
| `docker/*.Dockerfile` and their scripts | `scripts/build-images.sh`, the publish workflows, compose `build:` blocks, `docs/docs/overview/development.md` |

Everything that moves is test-only and referenced only from `scripts/`,
`compose/README.md`, `AGENTS.md`, the `Justfile`, the workflows, and the
`docker-compose.e2e*.yaml` row of `docs/docs/setup/artifacts/docker-images.md`.
The moves update those in the same PR and leave no wrappers: contributors and
CI are the only callers.

## The build runner

`build-runner.md` replaces the patched `makechrootpkg` with AURCache's own
runner; from its step 2 the PKGBUILD no longer runs on the host at all. The
`build-isolation` suite is written against the worker's entry point
(`aurcache-build --interface=N`, whatever is behind it) and against
outcomes: the hostile fixture's attempts fail, and an unconfined control lets
the same fixture succeed. It asserts nothing about Landlock or the patch. When
the runner lands, the confined leg keeps passing through the same entry
point; the Landlock-specific expectations (#450's `RESOLVE_UNIX`) become
"a build not offered the agent cannot reach it", whatever enforces it. The
unconfined control can stay stock `makechrootpkg` for as long as devtools is
installable; it only proves the fixture is hostile.

## Migration

After #435, #438, #446, #447, #450, #451 and #452 land: they edit the
scripts, compose files and fixtures that move. Each step is one PR and keeps
CI green alone.

1. **Fixtures.** Move `docker/testpkg/`, `docker/git-ssh*` (and #451's
   `git-fixtures.Dockerfile`) and `scripts/fixtures/` under
   `tests/fixtures/`. Paths only. Verifies: per-push jobs pass; one
   `run-e2e` and one `run-e2e-kube` run pass; `rg` finds no old path.
2. **Component suites.** `test-kernel.sh`, `test-frontend.sh`,
   `test-sandbox.sh`, `test-build-agent.sh` to `tests/*/run.sh`, `just`
   targets, `AGENTS.md`. Verifies: the same CI jobs pass with the same test
   counts.
3. **CI compilation** (D10). Kernel tests in the Rust job from its build.
   Verifies: the same four test binaries run; per-push runner time drops.
4. **Harness and reference topology.** `tests/e2e/run.sh`, the `chroot`
   topology from `compose/docker-compose.yaml` plus overlay, `basic`. The
   nightly calls it. Delete `scripts/test-e2e.sh` and
   `docker-compose.e2e.yaml`. Verifies: a `run-e2e` run builds and installs
   `hello`, and the nginx repository service serves it.
5. **Hybrid topologies** (D1, D2). Their compose files into
   `topologies/hybrid{,-host}/`; delete both wrappers and both `.dev.yaml`;
   `basic` on both nightly. Verifies: both columns pass.
6. **SSH** (D4-D7). The `gitssh` fixture overlay and `ssh-credentials`;
   the API test for the clone refusal and the worker test for the
   not-listed log line. Delete `test-e2e-ssh.sh` and its overlay.
   Verifies: the scenario passes, and fails with #452's fix reverted.
7. **Kube topology** (D9, D12). The kind driver behind the topology hooks;
   `test-e2e-kube.sh`'s legs become scenarios, three of them shared. One
   nightly workflow with an image job. Verifies: every former leg reports
   pass or skip in the summary; the nightly takes no longer than today's two
   workflows in parallel.
8. **Hostile fixtures** (D8). One set; `build-isolation` runs it through
   both executors' entry points, `kube` through its pods. Verifies: each leg
   still fails with its executor's confinement disabled.
9. **`compose/` and docs.** Drop the e2e table from `compose/README.md` and
   the e2e row from `docker-images.md`; `tests/README.md` takes them.
   Verifies: the docs build; `aurcli setup compose`'s comparison test passes.

Then the scenarios listed in `e2e-scenarios.md`, one file each.

## Open questions

- **`tests/` at the root**, or `scripts/tests/` to keep the root as it is?
  This design assumes `tests/`.
- **The `.dev.yaml` files**: drop them for the topology files (D1), or keep
  developer conveniences in `compose/dev/`?
- **Reference topology from the user file** (D3): it tests what users run,
  but ties the e2e to that file's ports and services. Acceptable?
- **A Cargo cache in CI** (D11): allowed, or are clean builds per job
  deliberate?
- **Labels**: one `run-e2e` for the whole matrix (as long as the kube
  column, ~20-30 min) plus
  `run-e2e:<topology>`, replacing `run-e2e-kube`?
- **Hybrid topologies nightly or weekly?** They change rarely, and the
  hybrid image exists for upgrades.
- **The unconfined control** once the build runner lands: stock
  `makechrootpkg`, or a test-only unconfined mode of the runner?
