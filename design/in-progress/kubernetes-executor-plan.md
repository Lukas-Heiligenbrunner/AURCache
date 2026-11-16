# Kubernetes worker and shared build agent: implementation plan

Status: **In progress** · Last updated: 2026-11-16 · Branch
`feat/kubernetes-executor`, one pull request

The plan for building what `kubernetes-executor.md` and
`container-workers.md` design. Read those two first: this document says who
builds which part, against which interface, in which order, and how each
part is checked. It does not repeat why.

Contents:

1. [What the pull request delivers](#1-what-the-pull-request-delivers)
2. [Decisions made while planning](#2-decisions-made-while-planning)
3. [Crates and files](#3-crates-and-files)
4. [Interfaces, pinned](#4-interfaces-pinned)
5. [Rules for everyone working on this](#5-rules-for-everyone-working-on-this)
6. [Work packages](#6-work-packages)
7. [Waves](#7-waves)
8. [Integration](#8-integration)
9. [Test strategy](#9-test-strategy)
10. [Images and packaging](#10-images-and-packaging)
11. [Risks, and where to stop and ask](#11-risks-and-where-to-stop-and-ask)
12. [After this pull request](#12-after-this-pull-request)

---

## 1. What the pull request delivers

- **`aurcache-build-agent`**: the statically linked program that runs a
  build inside its container: a root step, a drop to a build user with no
  capabilities, a pacman helper that installs named dependencies and nothing
  else, a report.
- **The Docker worker, supported again and on the agent**: no `sudo`, no
  capabilities for build code, measurements reported. Called the Docker
  worker everywhere; no setting renamed.
- **`aurcache-worker-kube`**: a worker that builds each job in a pod of its
  own, at Pod Security `baseline`, and streams packages from the pod to the
  server without storing them.
- **An image** for it (`aurcache-worker-kube`), **manifests**
  (`kubernetes/kube-worker/`), **documentation**, and three new test
  suites: the agent in Docker, the dispatcher against a fake cluster, and
  the whole thing in kind.

Not in it: caches, persistent build directories, kept failed builds,
adoption of pods across a restart, a direct upload from the pod, SSH
credentials for the Docker worker, any change to the worker protocol, the
server, or the chroot worker's build path, and any change inside the AUR
submodules.

**The worker protocol does not change.** No type in
`aurcache-common/src/worker.rs` is touched and no fixture under
`aurcache-common/tests/fixtures/worker-protocol/` is edited. A work package
that finds it needs either stops and says so (section 11).

**No server environment variable is added.** Everything new is read by a
worker. So `aurcache_common::server_env` and
`docs/docs/Configuration/environment-variables.md` gain nothing, and the
test that ties them together is unaffected.

---

## 2. Decisions made while planning

The user's decisions are in `kubernetes-executor.md` ("What was decided").
These are the ones planning added, each argued in the design documents:

| decision | where it is argued |
|---|---|
| Packages are streamed with one `exec` per file (`send`), raw bytes, after a manifest of names, sizes and SHA-256; not one tar of everything. The server's upload route already takes it: **no protocol change** | `kubernetes-executor.md`, "Getting things in and out" |
| The report is a file read through `exec` (`manifest`), not the container's termination message, which is capped at 4096 bytes | same, "Inside the build container" |
| One agent binary for both workers, in a crate of its own, whose library half is the interface types | `container-workers.md`, "The build agent" |
| **The worker brings the agent; the builder image is not changed.** The Docker worker copies it into the build directory, the Kubernetes worker's init container copies it from the dispatcher's image | same, "Why a binary, and why the worker brings it" |
| The agent is linked statically against **glibc** (`-C target-feature=+crt-static` on the usual gnu target), not musl: it needs no toolchain the images do not already have | `kubernetes-executor.md`, "The agent as a static binary" |
| The worker's side of an agent-run build is a third crate, `aurcache-worker-container`, not a module of `aurcache-worker-core` | `container-workers.md`, "The worker's side" |
| The chroot worker's build path is left alone; two pure functions move to `aurcache-common` | same, "What the chroot worker shares" |
| User-facing name: **Docker worker**. Environment variables, setting keys, `kind`, binary and package names unchanged | same, "Naming" |
| Readiness asks the cluster with a server-side dry run of the pod, instead of reading `ResourceQuota` objects | `kubernetes-executor.md`, "Concurrency and `ready_for_work`" |
| Everything the executor asks of Kubernetes goes through one trait, `Cluster`, so the lifecycle is tested against a fake | same, "Testing" |
| The dispatcher manifests are a kustomization of their own (`kubernetes/kube-worker/`), not a component of the root one, whose `namespace:` would rename the builds' namespace | section 10 |
| Limits are unset by default on the Kubernetes worker; a set limit is also the request | `kubernetes-executor.md`, "Limits" |
| `WORKER_KUBE_USER_NAMESPACES` is the machine's, not a declared setting | same, "User namespaces" |
| The pod template is JSON, read into an allow-list of fields | same, "Arches" |

---

### Answered by the user after planning

- **Test cluster:** use `kind` on freyja, with its storage on a TrueNAS zvol attached over iSCSI (`/mnt/aurcache-scratch`, 1T btrfs). Mount node data there through `extraMounts` so it stays off the nearly full root filesystem, and put the `kind` binary in `/mnt/aurcache-scratch/tools/kind/`. A k3s VM on njord is also available: see `/mnt/aurcache-scratch/k8s/README.md`.
- **Build directories** go under `/mnt/aurcache-scratch/target/wt-k8s-<wp>/`, not `/mnt/cargo-target`.
- **Hybrid image:** stays a compatibility image. Only the Docker worker inside it is un-deprecated.
- **Docker worker:** switches to the new root-step-then-drop flow, with an upgrade note. The builder image must stay compatible with older workers that may still start it through its entrypoint and sudo, so the agent is brought by the worker and `docker/builder.Dockerfile` does not change.
- **Docs URL:** rename `legacy-docker` to `docker`, with `@docusaurus/plugin-client-redirects` keeping the old address working.
- **Defaults kept:** Kubernetes limits unset by default; no standalone Docker worker image in this PR; the NetworkPolicy probe (WP12) stays in scope, and is cut first if needed.

## 3. Crates and files

```
backend/
  aurcache-common/src/artifact.rs          new: is_artifact (moved)
  aurcache-common/src/source.rs            + ref_candidates (moved)
  aurcache-build-agent/                    new crate: lib (interface) + bin (the agent)
    src/lib.rs spec.rs report.rs layout.rs
    src/pacman_request.rs pacman_conf.rs makepkg_conf.rs      pure, tested in place
    src/main.rs                            the command line, nothing else
    src/agent/build/                       the build: root step, drop, helper, makepkg, report
    src/agent/transfer/                    stage, receive, hold, tail, manifest, send, probe
    tests/interface.rs tests/fixtures/interface-1/
    tests/transfer.rs
  aurcache-worker-container/               new crate: the worker's side of an agent-run build
    src/lib.rs spec.rs input.rs outcome.rs unsupported.rs agent.rs arch.rs
  aurcache-worker-docker/                  on the agent; src/commands.rs deleted
  aurcache-worker-kube/                    new crate: the dispatcher
    src/lib.rs main.rs
    src/config.rs settings.rs pod.rs       configuration and rendering (pure)
    src/cluster.rs                         the seam: trait and its types
    src/kube_cluster.rs                    the seam over kube-rs
    src/executor.rs transfer.rs outcome.rs the lifecycle
    src/probe.rs                           the NetworkPolicy probe
    tests/support/ tests/lifecycle.rs tests/streaming.rs
docker/
  kube-worker.Dockerfile build-kube-worker.sh        new
  testpkg/agent-hostile/                             new fixture
kubernetes/kube-worker/                              new manifests
scripts/
  test-build-agent.sh test-e2e-kube.sh               new
  fixtures/kube-e2e/                                 new
docs/docs/workers/kubernetes.md                      new
docs/docs/workers/docker.md                          renamed from legacy-docker.md
```

---

## 4. Interfaces, pinned

Work packages build against these before the code behind them exists. WP0
commits them as code; from then on a change to one is a change to this
section first, agreed with the orchestrator, because someone else is
compiling against it.

### 4.1 `aurcache-common`

```rust
// aurcache_common::artifact   (moved from aurcache_worker_core::artifacts, with its tests)
pub fn is_artifact(name: &str) -> bool;

// aurcache_common::source     (moved from aurcache_worker::built_sources::candidates, with its tests)
/// The refs a name may mean, in the order the server's `ls_remote` prefers them.
pub fn ref_candidates(git_ref: &str) -> Vec<String>;
```

`aurcache_worker_core::artifacts::is_artifact` stays, as
`pub use aurcache_common::artifact::is_artifact;`.

### 4.2 The agent's interface: `aurcache_build_agent` (library)

```rust
// spec.rs
/// What the agent and a worker must agree on to work together.
pub const INTERFACE: u32 = 1;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BuildSpec {
    /// `INTERFACE` of whoever wrote this; the agent refuses any other.
    pub interface: u32,
    pub pkgbase: String,
    /// Arguments added to makepkg's command line, as the job gave them.
    pub build_flags: Vec<String>,
    /// The job's makepkg assignments. Where things go and how many jobs run
    /// are the agent's: it drops such lines and sets its own after these.
    pub makepkg_conf: String,
    /// Whole, with the AURCache repository already appended by the worker.
    pub pacman_conf: String,
    pub mirrorlist: Option<String>,
    pub pgp_keys: Vec<String>,
    pub keyserver: String,
    pub vcs_sources: Vec<aurcache_common::worker::JobVcsSource>,
    /// The CPUs the build is limited to, which size its parallelism.
    pub cpus: Option<f64>,
    /// Where the build's own trees go, inside the container.
    pub scratch: PathBuf,
    pub credential: Option<Credential>,
}

/// An SSH credential for the build to use and never read: paths inside the
/// container that only root can open.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Credential {
    pub key: PathBuf,
    pub known_hosts: Option<PathBuf>,
}

// report.rs
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BuildReport {
    pub interface: u32,
    pub ended: Ended,
    pub peak_memory_bytes: Option<u64>,
    pub oom_kills: Option<u64>,
    /// Bytes under the work and scratch directories when the build ended.
    pub disk_bytes: Option<u64>,
    /// Keyed by `JobVcsSource::source_url`.
    pub vcs_commits: BTreeMap<String, String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "how", rename_all = "snake_case")]
pub enum Ended {
    Exited { code: i32 },
    Signaled { signal: i32 },
    Setup { step: SetupStep, message: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SetupStep { Spec, Config, Keyring, Sync, User, Source, Keys, Credential }

/// What `manifest` prints.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Handback {
    pub interface: u32,
    /// `None` when the build container ended without writing one.
    pub report: Option<BuildReport>,
    pub artifacts: Vec<Artifact>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Artifact { pub name: String, pub size: u64, /** Lowercase hex. */ pub sha256: String }

/// One line of what `probe` prints.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reach { pub target: String, pub reachable: bool }

// layout.rs
pub const BIN_NAME: &str = "aurcache-build-agent";
/// The capabilities the build container's root has, and no others.
pub const CAPABILITIES: [&str; 7] =
    ["CHOWN", "DAC_OVERRIDE", "FOWNER", "FSETID", "SETUID", "SETGID", "KILL"];
pub const BUILD_UID: u32 = 1001;
pub const BUILD_GID: u32 = 1001;
pub const INPUT_SPEC: &str = "spec.json";
pub const INPUT_SOURCE: &str = "source.tar.gz";
/// The variable makepkg's environment carries the helper's socket in.
pub const PACMAN_SOCKET_VAR: &str = "AURCACHE_PACMAN_SOCKET";

/// The directory a worker gives the agent.
pub struct Work(PathBuf);
impl Work {
    pub fn new(root: impl Into<PathBuf>) -> Self;
    pub fn root(&self) -> &Path;
    pub fn input(&self) -> PathBuf;    // <root>/in
    pub fn spec(&self) -> PathBuf;     // <root>/in/spec.json
    pub fn source(&self) -> PathBuf;   // <root>/in/source.tar.gz
    pub fn ready(&self) -> PathBuf;    // <root>/in/ready
    pub fn pkg(&self) -> PathBuf;      // <root>/pkg
    pub fn log(&self) -> PathBuf;      // <root>/log/build.log
    pub fn out(&self) -> PathBuf;      // <root>/out
    pub fn report(&self) -> PathBuf;   // <root>/out/report.json
}

/// The build's own trees, which no worker reads.
pub struct Scratch(PathBuf);
impl Scratch {
    pub fn new(root: impl Into<PathBuf>) -> Self;
    pub fn startdir(&self, pkgbase: &str) -> PathBuf;  // <root>/src/<pkgbase>
    pub fn src(&self) -> PathBuf;
    pub fn build(&self) -> PathBuf;
    pub fn srcdest(&self) -> PathBuf;
    pub fn home(&self) -> PathBuf;
    pub fn makepkg_conf(&self) -> PathBuf;             // <root>/makepkg.conf
    pub fn pacman_socket(&self) -> PathBuf;            // <root>/run/pacman.sock
    pub fn ssh_socket(&self) -> PathBuf;               // <root>/run/ssh-agent.sock
}

// pacman_request.rs
/// What root will install: validated targets, nothing of the caller's argv.
pub struct Install { pub targets: Vec<String> }
pub enum Refusal { /* one variant per reason; Display names the offending word */ }
/// `argv` is what makepkg ran after `PACMAN_AUTH`: pacman's path first.
pub fn validate(argv: &[String]) -> Result<Install, Refusal>;
/// The command line root runs for it.
pub fn command(install: &Install) -> Vec<String>;

// pacman_conf.rs
pub enum ConfRefusal { /* an option the agent sets itself; a file:// server */ }
pub fn check(text: &str) -> Result<(), ConfRefusal>;

// makepkg_conf.rs
pub struct Rendering<'a> {
    pub job: &'a str,
    pub work: &'a Work,
    pub scratch: &'a Scratch,
    pub cpus: Option<f64>,
    pub agent: &'a Path,
    pub ssh: Option<Ssh<'a>>,
}
pub struct Ssh<'a> { pub socket: &'a Path, pub known_hosts: Option<&'a Path> }
/// `source /etc/makepkg.conf`, the job's lines minus the layout ones, then the agent's.
pub fn render(rendering: &Rendering<'_>) -> String;
```

The JSON of `BuildSpec`, `BuildReport` and `Handback` is frozen as fixtures
(`tests/fixtures/interface-1/`). Within interface 1 these types only gain
optional fields, by the worker protocol's own rule; anything else is
interface 2.

### 4.3 The agent's command line

```
aurcache-build-agent --interface                         prints "1"
aurcache-build-agent build --work <dir> [--log-file <path>]
aurcache-build-agent pacman-request <argv...>            PACMAN_AUTH's client; exits with pacman's status, 126 when refused
aurcache-build-agent unpack --archive <file> --into <dir>      internal: run by `build`, as the build user
aurcache-build-agent stage --work <dir> --tools <dir>
aurcache-build-agent receive --work <dir>                stdin: tar
aurcache-build-agent hold
aurcache-build-agent tail --work <dir> --from <offset>   stdout: log bytes
aurcache-build-agent manifest --work <dir>               stdout: Handback JSON
aurcache-build-agent send --work <dir> --name <file>     stdout: the file
aurcache-build-agent probe --timeout <seconds> --connect <host:port>...   stdout: [Reach] JSON
```

`main.rs` (WP0) parses these with clap and calls, in `src/agent/`:

```rust
// agent/build/mod.rs            (WP1)
pub fn run(work: &Path, log_file: Option<&Path>) -> ExitCode;
pub fn pacman_request(argv: Vec<String>) -> ExitCode;
pub fn unpack(archive: &Path, into: &Path) -> ExitCode;
// agent/transfer/mod.rs         (WP2)
pub fn stage(work: &Path, tools: &Path) -> ExitCode;
pub fn receive(work: &Path) -> ExitCode;
pub fn hold() -> ExitCode;
pub fn tail(work: &Path, from: u64) -> ExitCode;
pub fn manifest(work: &Path) -> ExitCode;
pub fn send(work: &Path, name: &str) -> ExitCode;
pub fn probe(targets: &[String], timeout: Duration) -> ExitCode;
```

Wire formats, exactly:

- **`receive`**: stdin is an uncompressed tar holding the regular files
  `spec.json` and `source.tar.gz`, in either order, and nothing else; any
  other entry is refused. It stops reading at the tar's end-of-archive
  marker (it does not wait for end of input), writes both under `<work>/in/`,
  then creates `<work>/in/ready`.
- **`stage`**: copies its own executable to `<tools>/aurcache-build-agent`
  (mode 0755), creates `<work>/in/`, and exits 0 once `<work>/in/ready`
  exists.
- **`tail`**: writes `<work>/log/build.log` from byte `<offset>` and keeps
  following. It waits for the file to appear. It exits 0 when
  `<work>/out/report.json` exists and it has written everything up to the
  end of the log.
- **`manifest`**: one `Handback` as JSON on stdout. `artifacts` are the
  entries of `<work>/pkg/` that are regular files (opened `O_NOFOLLOW`) and
  pass `is_artifact`, sorted by name.
- **`send`**: refuses a name that fails `is_artifact`, opens
  `<work>/pkg/<name>` with `O_NOFOLLOW`, requires a regular file, and writes
  its bytes to stdout and nothing else. Diagnostics go to stderr.
- **`probe`**: tries a TCP connection to each target, in parallel, each for
  at most the timeout; prints `[Reach]`; exits 0 whatever it found.
- **`build`**: exits 0 if and only if it wrote `<work>/out/report.json`.

### 4.4 `aurcache_worker_container`

```rust
// spec.rs
pub struct SpecOptions {
    pub cpus: Option<f64>,
    pub keyserver: String,
    pub scratch: PathBuf,
    pub credential: Option<Credential>,
}
pub fn for_job(claimed: &ClaimedJob, repo: Option<&RepoSection>, options: SpecOptions) -> BuildSpec;

// input.rs
/// `<work>/in/spec.json` and `<work>/in/source.tar.gz`, for a worker that shares a directory.
pub fn write(work: &Path, spec: &BuildSpec, source: &[u8]) -> Result<()>;
/// The same two as the tar `receive` reads.
pub fn pack(spec: &BuildSpec, source: &[u8]) -> Result<Vec<u8>>;

// outcome.rs
/// A worker's own memory limit, for naming in an out-of-memory failure.
pub struct MemoryLimit { pub bytes: u64, pub variable: &'static str }
/// How a build container ended when the worker did not stop it.
pub enum Ending {
    Reported(BuildReport),
    /// The engine says the kernel's OOM killer ended the container.
    OomKilled,
    /// It ended with no report, for a reason that is not the package's.
    Unreported { detail: String },
}
pub fn complete(stop: Option<Stop>, elapsed: Duration, ending: Ending, memory: Option<MemoryLimit>)
    -> CompleteReport;
pub fn oom_reason(kills: Option<u64>, memory: Option<MemoryLimit>) -> String;

// unsupported.rs
/// Say, in the build log and to the server, what this job asked for that an
/// agent-run build does not do. Nothing, for a job that asked for none of it.
pub async fn warn(client: &WorkerClient, job: &JobDescriptor);

// agent.rs
pub const VARIABLE: &str = "WORKER_BUILD_AGENT";
pub const INSTALLED: &str = "/usr/lib/aurcache/bin/aurcache-build-agent";
/// The agent binary on this machine: `WORKER_BUILD_AGENT`, else beside the
/// running executable, else `INSTALLED`.
pub fn locate() -> Result<PathBuf>;

// arch.rs
/// An Arch architecture in the OCI vocabulary.
pub struct Platform { pub arch: &'static str, pub variant: Option<&'static str> }
pub fn platform(arch: &str) -> Option<Platform>;
```

`complete` maps, in this order: a `stop` wins
(`report::ended(Some(stop), ..)`); `Reported` with `Exited { 0 }` is a
success; `Exited`/`Signaled` otherwise is a failure through `report::ended`,
worded by `oom_reason` when `oom_kills > 0`; `Setup` is
`report::interrupted`; `OomKilled` is a failure worded by `oom_reason`;
`Unreported` is `report::interrupted`. Measurements from a report are copied
onto whatever it returns: `peak_memory_bytes`, `vcs_commits`, and
`disk_usage: Some(DiskUsage { workdir: Some(disk_bytes), ..None })`.

### 4.5 `aurcache_worker_kube`

```rust
// cluster.rs  (complete in WP0)
pub trait Cluster: Send + Sync + 'static {
    fn create(&self, pod: &Pod) -> impl Future<Output = Result<(), CreateError>> + Send;
    /// Whether the cluster would admit `pod` now: a server-side dry run.
    fn admits(&self, pod: &Pod) -> impl Future<Output = Admission> + Send;
    /// Delete one pod; one already gone is not an error.
    fn delete(&self, name: &str, grace: Duration) -> impl Future<Output = Result<()>> + Send;
    /// Delete every pod carrying this dispatcher's label; how many there were.
    fn sweep(&self) -> impl Future<Output = Result<usize>> + Send;
    /// The pod as the watch last saw it; `None` when there is none by that name.
    fn pod(&self, name: &str) -> Option<Arc<Pod>>;
    /// Every pod of this dispatcher, as the watch last saw them.
    fn pods(&self) -> Vec<Arc<Pod>>;
    /// A counter that moves whenever the watch changed anything.
    fn generation(&self) -> tokio::sync::watch::Receiver<u64>;
    fn exec(&self, pod: &str, container: &str, argv: &[&str], stdin: Stdin)
        -> impl Future<Output = Result<Exec>> + Send;
    /// The last `lines` lines a container wrote, from the kubelet's log.
    fn log_tail(&self, pod: &str, container: &str, lines: i64)
        -> impl Future<Output = Result<String>> + Send;
}

pub enum CreateError { Refused { message: String }, Exists, Other(anyhow::Error) }
pub enum Admission { Admitted, Refused { message: String }, Unknown { message: String } }
pub enum Stdin { Closed, Piped }
pub struct Exec {
    /// `Some` for `Stdin::Piped`.
    pub stdin: Option<Pin<Box<dyn AsyncWrite + Send>>>,
    pub stdout: Pin<Box<dyn AsyncRead + Send>>,
    /// Resolves once the process is over.
    pub ended: BoxFuture<'static, ExecEnd>,
}
pub enum ExecEnd {
    Succeeded,
    /// It exited non-zero; `message` is the status and the tail of its stderr.
    Failed { message: String },
    /// The stream broke before a status arrived.
    Lost { message: String },
}

// config.rs  (WP4)
pub fn core_from_env() -> CoreConfig;
pub struct Config {
    pub namespace: String,
    /// The label value every pod of this dispatcher carries: the first 16
    /// hex digits of its fingerprint.
    pub dispatcher: String,
    pub builder_image: String,
    pub pull_policy: String,
    pub agent_image: AgentImage,
    pub credential_secret: Option<String>,
    pub user_namespaces: bool,
    pub extras: PodExtras,
    pub network_probe: bool,
    pub tunables: Tunables,
}
pub enum AgentImage { Given(String), OwnPod { namespace: String, name: String } }
/// What each build takes from the declared settings.
pub struct Tunables {
    pub memory: Option<u64>, pub cpus: Option<f64>, pub disk: Option<u64>,
    pub schedule_timeout: Duration, pub keyserver: String,
}
impl Config {
    pub fn from_env(settings: &WorkerSettings, fingerprint: &str) -> Result<Self>;
    pub fn with_settings(&self, settings: &WorkerSettings) -> Self;
}

// settings.rs  (WP4)
pub mod keys { /* build_memory_max, build_cpus, build_disk_max, schedule_timeout, keyserver */ }
pub fn kube_settings() -> Vec<SettingSpec>;

// pod.rs  (WP4)
pub const MANAGED_BY: &str = "aurcache-worker-kube";
pub const BUILD: &str = "build";  pub const STAGE: &str = "stage";  pub const HOLD: &str = "hold";
pub const WORK: &str = "/work";
pub struct PodInputs<'a> {
    pub cfg: &'a Config,
    pub agent_image: &'a str,
    pub job: &'a JobDescriptor,
    pub timeout: Option<Duration>,
}
pub fn name(dispatcher: &str, build_id: i32) -> String;   // aurcache-build-<dispatcher>-<build_id>
pub fn selector(dispatcher: &str) -> String;              // aurcache.io/dispatcher=<dispatcher>
/// Labels every pod of this dispatcher carries, a build's or not.
pub fn labels(dispatcher: &str) -> BTreeMap<String, String>;
pub fn render(inputs: &PodInputs<'_>) -> Result<Pod>;     // Err: an architecture with no OCI name

// kube_cluster.rs  (WP5)
pub struct KubeCluster { /* a client, the namespace, the reflector's store */ }
impl KubeCluster {
    /// Connect (in-cluster, or `KUBECONFIG`), start the watch on this
    /// dispatcher's pods, and return once it has listed them.
    pub async fn connect(namespace: &str, dispatcher: &str) -> Result<Self>;
    /// The image a pod runs, by digest where its status has a pullable one.
    pub async fn image_of(&self, namespace: &str, pod: &str) -> Result<String>;
}
impl Cluster for KubeCluster { /* … */ }

// executor.rs  (WP9)
pub struct KubeExecutor<C: Cluster> { /* Live<Config>, Arc<C>, the agent image */ }
impl<C: Cluster> KubeExecutor<C> {
    pub fn new(cfg: Config, cluster: Arc<C>, agent_image: String) -> Self;
}
impl<C: Cluster> Executor for KubeExecutor<C> { const KIND: &'static str = "kubernetes"; /* … */ }

// probe.rs  (WP12)
pub struct Targets { /** `host:port` of the server's worker port. */ pub worker: String,
                     /** `host:port` of the repository, when there is one. */ pub repo: Option<String> }
pub enum ProbeOutcome {
    /// What must be closed is closed.
    Enforced,
    /// These answered and should not have.
    Open { reachable: Vec<String> },
    /// The probe could not be run or read.
    Inconclusive { reason: String },
}
pub async fn check<C: Cluster>(cluster: &C, cfg: &Config, agent_image: &str, targets: &Targets)
    -> ProbeOutcome;
```

### 4.6 The Kubernetes worker's environment

Beside what every worker reads (`AURCACHE_URL`, `WORKER_DATA_DIR`,
`WORKER_NAME`, `WORKER_ARCHES`, `AURCACHE_REPO_URL`, the protocol settings):

| variable | default | declared setting | |
|---|---|---|---|
| `WORKER_KUBE_NAMESPACE` | `aurcache-builds` | no | where build pods go |
| `WORKER_KUBE_BUILDER_IMAGE` | `ghcr.io/lukas-heiligenbrunner/aurcache-builder-v2:latest` | no | the build container's image |
| `WORKER_KUBE_IMAGE_PULL_POLICY` | `Always` | no | for the builder image; `IfNotPresent` for an image loaded by hand |
| `WORKER_KUBE_AGENT_IMAGE` | the dispatcher's own pod's image | no | the `stage` and `hold` containers' image |
| `WORKER_KUBE_POD_NAME`, `WORKER_KUBE_POD_NAMESPACE` | none; the manifests set them from the downward API | no | which pod to read that image from |
| `WORKER_KUBE_CREDENTIAL_SECRET` | none | no | a Secret in the builds' namespace: `ssh-privatekey`, optionally `known_hosts` |
| `WORKER_KUBE_USER_NAMESPACES` | `false` | no | `hostUsers: false` on build pods |
| `WORKER_KUBE_POD_TEMPLATE` | none | no | path of a JSON file: `tolerations`, `affinity`, `nodeSelector`, `priorityClassName`, `runtimeClassName`, `imagePullSecrets`, `schedulerName`, `topologySpreadConstraints`, `labels`, `annotations`; an unknown field is an error at startup |
| `WORKER_KUBE_NETWORK_PROBE` | `true` | no | run the NetworkPolicy probe at startup |
| `WORKER_BUILD_MEMORY_MAX` | unset | `build_memory_max`, Size, next job | the build container's memory request and limit |
| `WORKER_BUILD_CPUS` | unset | `build_cpus`, Float ≥ 0, next job | its CPU request and limit; sizes `MAKEFLAGS` |
| `WORKER_BUILD_DISK_MAX` | unset | `build_disk_max`, Size, next job | its `ephemeral-storage` request and limit, and the work volume's `sizeLimit`. The description says "evicted past this size, best effort" |
| `WORKER_KUBE_SCHEDULE_TIMEOUT` | `10m` | `schedule_timeout`, Duration, next job | how long a pod may stay unschedulable |
| `WORKER_KEYSERVER` | `aurcache_worker_core::settings::DEFAULT_KEYSERVER` | `keyserver`, Text, next job | where `validpgpkeys` are fetched |

The declared ones follow `worker-configuration.md`: the plain name pins,
`_DEFAULT` gives an overridable default, and the keys and kinds match the
chroot worker's where it has the same setting. The others are bootstrap,
host access or machine facts, and a test asserts none of them is declared
(as `aurcache-worker-docker/src/settings.rs` does).

The Docker worker gains one: `WORKER_BUILD_AGENT` (not declared), the path
of the agent binary, for a layout the default lookup does not cover.

### 4.7 Scripts and images

| name | what |
|---|---|
| `scripts/test-build-agent.sh [--keep]` | builds the static agent and runs it in the builder image under the pod's flags; `BUILDER_IMAGE` overrides the image |
| `scripts/test-e2e-kube.sh [package] [timeout]` | the kind suite; `KIND` and `KUBECTL` name the binaries |
| `AURCACHE_TEST_PREFIX` | read by both: the prefix of every container, image tag and cluster name they create (default `aurcache-test`) |
| image `aurcache-worker-kube` | `scripts/build-images.sh --images kube-worker`; `/usr/local/bin/aurcache-worker-kube` and `/usr/local/bin/aurcache-build-agent` |

The one command that builds the agent as it ships, used by every script and
image:

```bash
cd backend && RUSTFLAGS="-C target-feature=+crt-static" \
    cargo build --release --target "$TRIPLE" -p aurcache-build-agent
```

Always with `--target`, even for the host's own triple: without it the
flags reach build scripts and proc macros, which cannot be static.

---

### 4.8 Settled by the manifests (WP7)

The manifests in `kubernetes/kube-worker/` pin these, and the code must match them:

- **Dispatcher namespace:** `aurcache`, beside the server. It reads the `aurcache-enrollment` Secret there, and reaches the server at `aurcache.aurcache.svc`. Its ServiceAccount is `aurcache-kube-worker`, which may only `get` pods in `aurcache`: it reads its own pod for the image digest.
- **Build namespace:** `aurcache-builds`, at Pod Security `baseline`. Build pods set `serviceAccountName: aurcache-build`. That ServiceAccount has `automountServiceAccountToken: false` and no permissions.
- **RBAC in `aurcache-builds`:**
  - pods: create, get, list, watch, delete and `deletecollection` (`Cluster::sweep` uses a label-selected delete-collection);
  - pods/exec: create and get;
  - pods/log: get.
- **Dispatcher image:** `ghcr.io/lukas-heiligenbrunner/aurcache-worker-kube:<tag>`. Its default entrypoint starts `aurcache-worker-kube`, and the Deployment sets no `command`. It runs as uid/gid 65532, with `fsGroup` 65532, a read-only root filesystem and an emptyDir at `/tmp`. `docker/kube-worker.Dockerfile` (WP10) uses that uid, or leaves the uid to the pod spec, and the binary must not write outside `/tmp` and its data PVC.
- **Pod identity:** the downward API gives the dispatcher `POD_NAME` and `POD_NAMESPACE`.
- **Build-limit variables:** `WORKER_BUILD_MEMORY_MAX_DEFAULT`, `WORKER_BUILD_CPUS_DEFAULT` and `WORKER_BUILD_DISK_MAX_DEFAULT` are the names the manifests document. They follow section 4.6's `_DEFAULT` convention; WP4 confirms them or updates the manifests' comments.

## 5. Rules for everyone working on this

- **Read first**: `AGENTS.md` (its conventions are binding), then
  `kubernetes-executor.md` and `container-workers.md`, then your work
  package here. Navigate with `graft ask "<question>" --source`,
  `graft skeleton <file>`, `graft callers <symbol>` before grepping.
- **Your branch**: `k8s/<wp>` (for example `k8s/wp1`), in your own worktree,
  made from the tip of `feat/kubernetes-executor` *after the previous wave
  was merged* (wave 1 from the commit that adds this plan). Commit there.
  **Never push.** Do not touch another worktree.
- **Your files**: edit only the files your work package owns. If you need a
  change in someone else's file, or to an interface in section 4, stop and
  put it in your hand-back; do not make it.
- **`Cargo.toml` and `Cargo.lock` are WP0's.** If you find you need a
  dependency: add it at its latest stable version with a caret requirement
  (no pin, no pre-release), commit `Cargo.toml` and `Cargo.lock` alone in a
  commit titled `deps: ...`, and say so in your hand-back, so the
  orchestrator can merge it first and regenerate the lock for the others.
- **Builds**:
  ```bash
  export CARGO_TARGET_DIR=/mnt/cargo-target/aurcache/wt-k8s-<wp>/backend   # or .../frontend-rs
  export CARGO_BUILD_JOBS=4 CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0
  ```
  Never build into `/tmp` or the session scratchpad: they are a small tmpfs,
  and filling it breaks every shell. Build and test your own crates
  (`-p <crate>`), not the workspace, unless your work package says
  otherwise: the disk the target directories share has about 80 GB free.
  When your branch is merged, your target directory is removed.
- **Formatting**: `cargo fmt -p <your crate>` or `rustfmt <your files>`. No
  workspace-wide format.
- **Docker**: name every container, image tag, volume and network with your
  prefix (`wt-k8s-<wp>-...`; the new scripts read `AURCACHE_TEST_PREFIX`),
  and remove them by that name or id when you are done. Never
  `docker system prune`, never `pkill -f`. Builds that Docker runs write
  into Docker's storage, not into `/tmp`.
- **No system packages.** No `pacman -S`, no `sudo`. If a tool is missing,
  say so.
- **Protocol 1 is frozen.** Do not edit `aurcache-common/src/worker.rs` or
  anything under `aurcache-common/tests/fixtures/worker-protocol/`.
- **Code style**, from the project's own rules:
  - a comment makes sense read on its own: no issue numbers, no
    "previously", no comparison with another version, nothing about how the
    code came to be;
  - one role per function, short names, the signature says what it does;
  - an outcome is a named enum; `bool` is for yes/no questions only;
  - no tuple return types: a struct with named fields, in tests too;
  - no database writes, and nothing server-side, anywhere in this work;
  - user-facing text says **Docker worker** and **Kubernetes worker**.
- **Commits** end with:
  ```
  Co-Authored-By: Claude Fable 5.2 <noreply@anthropic.com>
  Claude-Session: https://claude.ai/code/session_01CWQocWQETSbcBqgaTqZ2bm
  ```
- **Your hand-back** says: the branch and its commits; what you verified,
  with the commands and their results; what you could not verify and why;
  every place you departed from this plan; anything you needed from another
  work package's files.

---

## 6. Work packages

| id | title | model, effort | wave | depends on |
|---|---|---|---|---|
| WP0 | Foundation: crates, interfaces as code, the two moves | opus, high | 1 | none |
| WP1 | The agent: the build | fable, high | 2 | WP0 |
| WP2 | The agent: transfer and probe subcommands | sonnet, high | 2 | WP0 |
| WP3 | The worker's side (`aurcache-worker-container`) | sonnet, medium | 2 | WP0 |
| WP4 | Kubernetes: configuration, settings, pod rendering | sonnet, medium | 2 | WP0 |
| WP5 | Kubernetes: the cluster seam over kube-rs | opus, high | 2 | WP0 |
| WP6 | The Docker worker is supported: names, docs, UI | sonnet, low | 2 | none |
| WP7 | Kubernetes manifests | sonnet, medium | 2 | none |
| WP8 | The Docker worker on the agent | opus, medium | 3 | WP1, WP3 |
| WP9 | Kubernetes: the executor | fable, high | 3 | WP2, WP3, WP4, WP5 |
| WP10 | Images, packaging, CI | opus, medium | 3 | WP1, WP2 |
| WP11 | Documentation: the Kubernetes worker page | sonnet, medium | 3 | WP4, WP7 |
| WP12 | Kubernetes: the NetworkPolicy probe | sonnet, medium | 3 | WP2, WP4, WP5 |
| WP13 | The kind suite | opus, high | 4 | everything above |
| WP14 | Integration | fable, high | 4, last | everything |

Every work package's acceptance includes: `cargo fmt --check` and
`cargo clippy --all-targets -- -D warnings` clean for the crates it touched,
its tests passing, and only its own files changed.

### WP0. Foundation

**Goal.** Everything later work packages compile against exists, compiles,
and is frozen: the three new crates in the workspace, section 4 as code, and
the lockfile settled so nobody else has to touch it.

**Owns.** `backend/Cargo.toml`, `backend/Cargo.lock`; every `Cargo.toml` of
the three new crates and of `aurcache-worker-docker` (dependencies only);
`aurcache-common/src/{artifact.rs,lib.rs,source.rs}`;
`aurcache-worker-core/src/artifacts.rs`;
`aurcache-worker/src/built_sources.rs`; and, as their first author, every
file of the three new crates listed in section 3.

**Does.**

- Moves `is_artifact` and its tests to `aurcache_common::artifact`, and
  `candidates` and its tests to `aurcache_common::source::ref_candidates`;
  re-exports the first from worker-core; makes the chroot worker call the
  second.
- `aurcache-build-agent`: `Cargo.toml` (`publish = false`, version `0.1.0`,
  `[lib]` and `[[bin]]`; dependencies: `anyhow`, `serde`, `serde_json`,
  `clap` with `derive`, `libc`, `tar`, `flate2`, `sha2`, `hex`,
  `aurcache-common` with `default-features = false`; dev: `tempfile`).
  `spec.rs`, `report.rs`, `layout.rs` complete, with the fixture test.
  `main.rs` complete. The three pure modules and the two `agent/`
  directories as stubs with the pinned signatures.
- `aurcache-worker-container`: `Cargo.toml` (`aurcache-common`,
  `aurcache-worker-core`, `aurcache-build-agent`, `anyhow`, `tokio`,
  `tracing`, `serde_json`, `tar`; dev: `tempfile`, `flate2`), `lib.rs`,
  stubs.
- `aurcache-worker-kube`: `Cargo.toml` (version `0.1.0`; `kube` 5.1 with
  `default-features = false` and `client`, `runtime`, `rustls-tls`, `ws`;
  `k8s-openapi` 0.29 with `v1_33`; `tokio`, `tokio-util` with `io`,
  `futures`, `anyhow`, `tracing`, `serde`, `serde_json`, `sha2`, `hex`, the
  three AURCache worker crates and `aurcache-common`; dev-dependencies as
  `aurcache-worker`'s, which `tests/streaming.rs` needs to start the real
  worker listener in-process: `aurcache-api`, `aurcache-db` with `testing`,
  `aurcache-ca`, `aurcache-utils`, `aurcache-activitylog`, `sea-orm`, `zstd`,
  `tar`, `tempfile`, and `tokio` with `test-util`). `cluster.rs` complete.
  Everything else as stubs.
- `aurcache-worker-docker/Cargo.toml`: adds `aurcache-build-agent`,
  `aurcache-worker-container`, `serde_json`.
- A stub is a file with its module documentation and the pinned public
  items, bodies `unimplemented!()`, that compiles without warnings.

**Tests.** `aurcache-build-agent/tests/interface.rs`: each fixture
deserializes, and serializes back to the same JSON; a spec with an unknown
field still reads. The moved tests pass where they now live.

**Verify.**

```bash
cd backend
cargo clippy --all-targets -- -D warnings          # the whole workspace, this once
cargo test -p aurcache-common -p aurcache-worker-core -p aurcache-build-agent \
           -p aurcache-worker-container -p aurcache-worker-kube
cargo test -p aurcache-worker --lib built_sources
cargo check -p aurcache-common --no-default-features && cargo check -p aurcache-client
cargo tree -p aurcache-build-agent -e normal | grep -E 'tokio|reqwest|rustls|sea-orm' && exit 1 || true
cargo tree -p aurcache-worker-kube -i rustls        # one version
RUSTFLAGS="-C target-feature=+crt-static" cargo build --release \
    --target x86_64-unknown-linux-gnu -p aurcache-build-agent
file "$CARGO_TARGET_DIR/x86_64-unknown-linux-gnu/release/aurcache-build-agent"   # static-pie linked
```

**Accepts when** all of that passes, the workspace has one rustls crypto
provider (the one `reqwest` already brings), and section 4 matches the code
item for item. Where `kube` 5.1's real API forces a different shape on
`cluster.rs` (the exec's stream types, most likely), WP0 changes this
section in the same commit and says so in its hand-back.

### WP1. The agent: the build

**Goal.** `aurcache-build-agent build` does what `container-workers.md`
("`build`, step by step") says, and the helper refuses everything but what
`kubernetes-executor.md` ("Installing what makepkg asks for") allows.

**Owns.** `aurcache-build-agent/src/{pacman_request.rs,pacman_conf.rs,makepkg_conf.rs}`,
`aurcache-build-agent/src/agent/build/` (all of it),
`scripts/test-build-agent.sh`, `docker/testpkg/agent-hostile/`.

**Provides.** `agent::build::{run, pacman_request, unpack}`; the three pure
modules. **Consumes.** `spec`, `report`, `layout`; `is_artifact` and
`ref_candidates` from `aurcache-common`.

**Notes.**

- Synchronous code, standard library and `libc` only: threads for the
  output pumps and the helper's socket, no async runtime. It is PID 1: it
  reaps, and it handles `SIGTERM` by killing everything and exiting.
- One function makes "this command, as the build user"
  (`setgroups`/`setresgid`/`setresuid` through `Command::uid`/`gid`, and
  `PR_SET_NO_NEW_PRIVS` in `pre_exec`); every step after the root step goes
  through it, so no step can forget the drop.
- The helper builds pacman's command line from `Install` alone, runs it
  with the agent's own environment and `/dev/null` for stdin, sends pacman's
  output to the same sink as makepkg's, and answers the client with the
  exit status. It serves one request at a time.
- Root never opens a path the build user controls without `O_NOFOLLOW`, and
  never `chown`s by path into a directory the build user can write while the
  build user has processes (the hand-back runs after the kill).
- The cgroup files are found through `/proc/self/cgroup`.

**Tests.**

- `pacman_request`: a table of what makepkg really sends (with and without
  each `PACMAN_OPTS` word, versioned targets, split packages) accepted, and
  a table refused: `-U`, `-Rnu`, `-Sy`, `-S` without `--asdeps`, `--config`,
  `--root`, `--dbpath`, `--overwrite`, `--hookdir`, a path, `repo/name`, a
  target starting with `-`, an empty target, a name with a shell
  metacharacter, no target at all. `command` never contains a word of the
  input that is not a validated target.
- `pacman_conf`: each forbidden option, in any case and spacing, and a
  `file://` server, refused; Arch's stock file accepted.
- `makepkg_conf`: layout lines of the job dropped; the agent's lines last;
  the parallelism rule (`-j$(nproc)` without a limit, `ceil` with one,
  never 0); the credential lines only with a credential; a hostile path
  survives a real shell as one value.
- `scripts/test-build-agent.sh`, the test that matters most. It builds the
  static agent, and with
  `docker run --user 0 --security-opt no-new-privileges --cap-drop=ALL --cap-add=<the seven>`
  on the published builder image:
  1. builds `docker/testpkg/hello`: the report says `Exited { code: 0 }`,
     has a peak memory figure, and one package is in `pkg/`, owned by the
     work directory's owner;
  2. builds `docker/testpkg/agent-hostile`, whose `makedepends` names a
     small package that is not in the image, and whose `build()` prints one
     line per probe: its `CapEff` is 0 and `NoNewPrivs` is 1; `sudo -n true`
     fails; it cannot read the credential file the script mounted; it
     cannot create or replace `out/report.json`, nor rename `out/`; the
     helper refuses `-U` of a package it made, `--config`, and a path; the
     dependency **was** installed (the positive leg). The script asserts
     each line;
  3. kills a build mid-way with `docker kill` and checks nothing is left
     and no report was written;
  4. where `/proc/sys/fs/binfmt_misc/qemu-aarch64` exists and the arm64
     builder image can be pulled, runs leg 1 with `--platform linux/arm64`
     and the x86_64 agent; skipped, saying so, otherwise.

**Verify.**

```bash
cd backend && cargo clippy -p aurcache-build-agent --all-targets -- -D warnings \
           && cargo test -p aurcache-build-agent
AURCACHE_TEST_PREFIX=wt-k8s-wp1 ./scripts/test-build-agent.sh
```

**Accepts when** both pass, and the script fails when the helper's
validation is replaced by `Ok` (try it, and say so in the hand-back: a test
of denials means something only if it can fail).

### WP2. The agent: transfer and probe subcommands

**Goal.** The other end of every `exec` the Kubernetes worker makes, exactly
as section 4.3 words it.

**Owns.** `aurcache-build-agent/src/agent/transfer/` (all of it),
`aurcache-build-agent/tests/transfer.rs`.

**Provides.** `agent::transfer::{stage, receive, hold, tail, manifest, send,
probe}`. **Consumes.** `layout`, `report`, `is_artifact`.

**Notes.** `send` and `tail` write with a fixed buffer and stop on a write
error (the reader went away). `manifest` hashes with a fixed buffer too: a
package may be larger than memory. `receive` refuses an entry that is not a
regular file or whose name is not one of the two, and never follows a path
out of `<work>/in/`. `hold` sleeps until signalled and exits 0 on `SIGTERM`.

**Tests** (`tests/transfer.rs`, running the built binary,
`env!("CARGO_BIN_EXE_aurcache-build-agent")`, against temporary
directories): `receive` round-trips what `aurcache_worker_container::input::pack`
will produce (build the tar by hand here; WP3 has the matching test), stops
at the end-of-archive marker with stdin still open, and refuses a third
entry, a symlink entry and `../x`; `stage` installs the binary and exits
only once `ready` exists; `tail --from N` resumes mid-file, follows appended
bytes, and ends once the report exists; `manifest` lists only regular files
with package names, with the right size and digest, and reports
`report: None` without one; `send` refuses a symlink named like a package
and a name with a slash; `probe` reports an open local port reachable and a
closed one not.

**Verify.** `cd backend && cargo clippy -p aurcache-build-agent --all-targets -- -D warnings && cargo test -p aurcache-build-agent --test transfer`

**Accepts when** those pass and no subcommand reads a whole package or a
whole log into memory.

### WP3. The worker's side

**Goal.** `aurcache-worker-container` as section 4.4 pins it.

**Owns.** `aurcache-worker-container/src/{spec.rs,input.rs,outcome.rs,unsupported.rs,agent.rs,arch.rs}`.

**Consumes.** `aurcache_worker_core::{executor::ClaimedJob, repo, report,
protocol::{Stop, log, report_warning}, client::WorkerClient}`; the agent's
library.

**Tests.** `for_job` appends the repository section and carries every field
of the job the agent needs; `pack` is read back by `tar` as exactly the two
entries, and `write` produces the same two files; `complete` for each row of
its mapping (a stop wins over a report; success carries the measurements; a
non-zero exit with OOM kills is worded as out of memory and names the
variable; a setup failure is `Interrupted` and names the step; `OomKilled`
is `Failed`; `Unreported` is `Interrupted`); `oom_reason` with and without a
limit, one kill and several; `platform` for the four architectures and an
unknown one; `locate` prefers the variable, then the sibling, then the
installed path, and its error names all three.

**Verify.** `cd backend && cargo clippy -p aurcache-worker-container --all-targets -- -D warnings && cargo test -p aurcache-worker-container`

### WP4. Kubernetes: configuration, settings, pod rendering

**Goal.** From the environment and a job to a `Pod`, with nothing in
between that needs a cluster.

**Owns.** `aurcache-worker-kube/src/{config.rs,settings.rs,pod.rs}`.

**Provides.** Section 4.5's `config`, `settings` and `pod` items, and the
environment of section 4.6. **Consumes.** `aurcache_build_agent::layout`
(the capability list, the binary's name), `aurcache_worker_container::arch`.

**Notes.** `render` produces the pod in `kubernetes-executor.md` ("The build
pod"), field for field. `core_from_env` advertises the dispatcher's own
architecture unless `WORKER_ARCHES` says otherwise. `PodExtras` is a struct
of the allowed fields with `deny_unknown_fields` (it is a local file, not a
protocol message); the operator's `nodeSelector` is merged under the
architecture and OS keys, and their labels under the dispatcher's.

**Tests.** The settings table checks out (`settings::check_table`), and none
of the undeclared variables is declared. Rendering: the snapshot of one
whole pod as JSON; no rendering, over every combination of settings and a
template that tries, has `privileged`, a `hostPath`, `hostNetwork`,
`hostPID`, `hostIPC` or an `fsGroup`; the build container has exactly
`layout::CAPABILITIES` and `allowPrivilegeEscalation: false`; `stage` and
`hold` are non-root with everything dropped; `hostUsers` is `false` only
when asked; each resource line appears only with its limit, and a request
always equals its limit; the credential volume appears only with a Secret
and is mounted into `build` alone; `activeDeadlineSeconds` follows the
timeout; names and label values are valid (63 characters, the DNS label
alphabet) for the largest build id; an unknown architecture is an error.

**Verify.** `cd backend && cargo clippy -p aurcache-worker-kube --all-targets -- -D warnings && cargo test -p aurcache-worker-kube --lib config settings pod`

### WP5. Kubernetes: the cluster seam over kube-rs

**Goal.** `KubeCluster`: the `Cluster` trait against a real API server.

**Owns.** `aurcache-worker-kube/src/kube_cluster.rs`.

**Notes.**

- One `kube::runtime` reflector on pods in the namespace with the
  dispatcher's label selector feeds `pod`, `pods` and `generation`. A watch
  that ends is restarted with backoff; `connect` returns after the first
  list.
- `exec` returns the process's stdout as a stream that is read only as fast
  as the caller reads it (kube's attached process is a bounded pipe; keep
  its buffer small, 64 KiB at most). stderr is collected up to 4 KiB for
  `ExecEnd::Failed`. Nothing is buffered whole.
- `admits` is `create` with `dryRun=All`. A `403`/`422` with a status
  message is `Refused`; a transport failure is `Unknown`, which the caller
  treats as "ask again", not as a refusal.
- `create` maps `409` to `Exists` and an admission refusal to `Refused`.
- `sweep` is `delete_collection` with the label selector and a grace period
  of 0.
- `image_of` returns the first container's `imageID` when it names a
  registry and a digest (stripping a `docker-pullable://` prefix), else that
  container's `image`.
- Check the API against docs.rs for the version in the lockfile, not from
  memory.

**Tests.** Unit tests for the pure parts: `image_of`'s choice from a
status, and the mapping of API errors.

And `#[ignore]` tests, run with `AURCACHE_KUBE_TESTS=1` against whatever
`KUBECONFIG` points at: create a `busybox` pod with the dispatcher's label;
see it through the watch; exec `cat` with piped stdin and read it back; exec
`head -c 200M /dev/zero` and read it slowly, without this process's memory
growing; read `ExecEnd::Failed` from `false`; dry-run a privileged pod into
a `baseline` namespace and get `Refused`; delete; sweep. Run them if a
`kind` binary is allowed (section 11, question 1), on a throwaway cluster
named with your prefix. If not, say in the hand-back that nothing has yet
exercised this file against a real API server.

**Verify.** `cd backend && cargo clippy -p aurcache-worker-kube --all-targets -- -D warnings && cargo test -p aurcache-worker-kube --lib kube_cluster`

### WP6. The Docker worker is supported: names, docs, UI

**Goal.** Nothing outside its crate still calls the Docker worker legacy,
deprecated, or a compatibility builder, and the new crates are described
where the others are.

**Owns.** `frontend-rs/src/screens/workers.rs`;
`docs/docs/workers/{legacy-docker.md,docker.md,hybrid.md,managing.md,index.md,split-chroot.md}`,
`docs/docs/overview/upgrading.md`,
`docs/docs/Configuration/environment-variables.md`,
`docs/docs/setup/artifacts/docker-images.md`, `docs/docusaurus.config.*`,
`docs/package.json`, `docs/yarn.lock`; `README.md`, `AGENTS.md`,
`compose/README.md`, comments in `compose/docker-compose.e2e-hybrid*.yaml`,
`docker/hybrid-entrypoint.sh`, `docker/arch.Dockerfile`,
`scripts/test-e2e.sh`, `scripts/test-e2e-hybrid.sh`,
`scripts/test-e2e-hybrid-legacy.sh`; comments in
`backend/aurcache-common/src/api/builds.rs`,
`backend/aurcache-worker-core/src/{lib.rs,executor.rs,settings.rs}`,
`backend/aurcache-worker/src/settings.rs`.

**Does.** Find them with `graft grep "deprecat"`, `graft grep "legacy"`,
`graft grep "compatibility builder"`; `legacy` about 0.5.0's activity rows,
the CA's old identities and the chroot pool's tests is another subject and
stays.

- `workers.rs`: `KindBadge` no longer singles out `docker`; one badge style
  and one title for every kind.
- `legacy-docker.md` becomes `docker.md`, "Docker worker", with the warning
  gone and the text rewritten from `container-workers.md` ("The Docker
  worker on the agent"): what it is for, what a build runs as, what it
  reports, what it does not support, its variables (unchanged, plus
  `WORKER_BUILD_AGENT`), other architectures without the paragraph about
  qemu's `C` flag. The old address redirects
  (`@docusaurus/plugin-client-redirects`, at the version of the other
  Docusaurus packages; section 11, question 2).
- `index.md`: the table reads "Docker worker … Supported", gains a
  "Kubernetes worker" row linking `./kubernetes.md` (WP11 writes that page),
  and the chroot worker's row no longer says it is what every setup wants.
  `managing.md`: the Type table has `docker` without "deprecated" or
  "highlighted", and gains `kubernetes`. `docker-images.md` gains
  `aurcache-worker-kube`.
- `hybrid.md`, `README.md`, `hybrid-entrypoint.sh`, `arch.Dockerfile`: the
  hybrid **image** stays described as the compatibility image for compose
  files from before the split (section 11, question 3); the worker inside
  it is "the Docker worker".
- `AGENTS.md`: the architecture bullet for the workers names
  `aurcache-worker-docker` as the per-build container executor,
  `aurcache-worker-kube` as the pod-per-build one, `aurcache-build-agent`
  and `aurcache-worker-container` as what they share. (WP14 adds the test
  suites to "Which end-to-end suite to run".)
- Comments are reworded to describe what is, with no "no longer".

**Verify.**

```bash
cd frontend-rs && CARGO_TARGET_DIR=/mnt/cargo-target/aurcache/wt-k8s-wp6/frontend-rs \
    cargo clippy --target wasm32-unknown-unknown --all-targets -- -D warnings && cargo test
cd backend && cargo check -p aurcache-common -p aurcache-worker-core -p aurcache-worker
cd docs && yarn install --frozen-lockfile && yarn build     # the link to kubernetes.md breaks until WP11; say so
```

**Accepts when** `graft grep` for the three words finds nothing about this
worker outside `backend/aurcache-worker-docker/`, `packaging/` and
`design/`.

### WP7. Kubernetes manifests

**Goal.** `kubectl apply -k kubernetes/kube-worker/` gives a cluster that
already runs the AURCache server a Kubernetes worker.

**Owns.** `kubernetes/kube-worker/` (new): `kustomization.yaml`,
`namespace.yaml`, `rbac.yaml`, `networkpolicy.yaml`, `worker.yaml`,
`README.md`.

**Notes.** Section 10 has the layout and why it is a kustomization of its
own. Read the manifests it sits beside:
`git show docs/199-kubernetes:kubernetes/kustomization.yaml` (and
`server/server.yaml`, `worker/worker.yaml`, `namespace.yaml`), and match
their labels (`app.kubernetes.io/name`, `component`, `part-of`), their
commenting style and their naming. Resources name their namespaces
explicitly. `worker.yaml`: a ServiceAccount, a 1 Gi `ReadWriteOnce` PVC, and
a Deployment (`replicas: 1`, `strategy: Recreate`, `enableServiceLinks:
false`) whose pod passes `restricted` (non-root, read-only root filesystem,
everything dropped, `RuntimeDefault` seccomp, an `fsGroup` for the PVC),
with `AURCACHE_URL=https://aurcache.aurcache.svc:8083`,
`AURCACHE_REPO_URL=http://aurcache.aurcache.svc:8081`, the optional
enrollment token Secret, `WORKER_CONCURRENCY_DEFAULT`, and
`WORKER_KUBE_POD_NAME`/`_NAMESPACE` from the downward API. The limits, the
credential Secret, user namespaces and the pod template are there commented,
each with a sentence. `rbac.yaml` and `networkpolicy.yaml` are the ones in
`kubernetes-executor.md`.

**Verify.** `kubectl kustomize kubernetes/kube-worker/` renders;
`kubectl apply --dry-run=client -k kubernetes/kube-worker/` accepts it. No
cluster is needed.

### WP8. The Docker worker on the agent

**Goal.** `aurcache-worker-docker` builds through the agent, as
`container-workers.md` ("The Docker worker on the agent") describes, and
its own text says what it is.

**Owns.** `aurcache-worker-docker/src/` (all of it; `commands.rs` is
deleted) and comments in `aurcache-worker-docker/Cargo.toml`.

**Consumes.** `aurcache_worker_container::{spec, input, outcome,
unsupported, agent, arch}`, `aurcache_build_agent::layout`.

**Notes.**

- A pure function renders the container (`ContainerCreateBody`) from the
  configuration, the job and the directories, so it can be tested; the
  executor only calls the engine.
- The build directory is created 0755, with `in/` written by
  `input::write` and the agent copied to `bin/` (0755). The only bind is the
  build directory at `/work`. Scratch is `/var/lib/aurcache-build`.
- `user: "0"`, `cap_drop: ["ALL"]`, `cap_add: layout::CAPABILITIES`,
  `security_opt: ["no-new-privileges"]`, `entrypoint` the agent, `cmd`
  `build --work /work`.
- After the wait: `out/report.json` if it parses, else the engine's
  `State.OOMKilled`, else `Unreported` with the exit code.
  `MemoryLimit { variable: "MEMORY_LIMIT" }` when one is set; `cpus` from
  `CPU_LIMIT`.
- `describe_self` is `docker (<image>)`. The startup warning goes. Crate and
  module documentation describe the worker as it is.
- `docker_arch` is replaced by `arch::platform`.
- Upload stays `upload_artifacts` from `pkg/`.

**Tests.** The rendered container: never `privileged`; exactly the seven
capabilities; `no-new-privileges`; user 0; one bind; the limits as before
(the existing `Tunables` tests stay). The directory preparation in a
temporary directory. Then run it for real, without a server: a small
`#[ignore]` test (run with `AURCACHE_DOCKER_TESTS=1`) that renders a
container for `docker/testpkg/hello`'s source, runs it against the local
daemon with a statically built agent, and reads a successful report and one
package.

**Verify.**

```bash
cd backend && cargo clippy -p aurcache-worker-docker --all-targets -- -D warnings \
           && cargo test -p aurcache-worker-docker
# build the static agent (section 4.7), then:
AURCACHE_DOCKER_TESTS=1 WORKER_BUILD_AGENT=<the static agent> AURCACHE_TEST_PREFIX=wt-k8s-wp8 \
    cargo test -p aurcache-worker-docker -- --ignored
```

The hybrid end-to-end suites are WP14's to run: they need the image, which
needs WP10.

### WP9. Kubernetes: the executor

**Goal.** `KubeExecutor`: a claimed job becomes a pod, a build log, streamed
packages and one report, as `kubernetes-executor.md` describes from "Shape"
to "How a build ended".

**Owns.** `aurcache-worker-kube/src/{executor.rs,transfer.rs,outcome.rs,main.rs,lib.rs}`,
`aurcache-worker-kube/tests/`.

**Consumes.** `Cluster`; `config`, `pod`; `KubeCluster::{connect,
image_of}`; `probe::check`; `aurcache_worker_container`; worker-core's
`Runner`, `BuildLog`, `WorkerClient`, `report`, `enroll`, `Live`.

**Notes.**

- `main.rs`: tracing; `core_from_env`; the identity; `Config::from_env`;
  `KubeCluster::connect`; the agent image (given, or `image_of` the
  dispatcher's own pod); the warning about user namespaces when off;
  `sweep`; enroll; the probe when enabled, its outcome logged and, when
  `Open`, reported to the server at error severity; `Runner::run`.
- `run_job`, in order: `unsupported::warn`; download the source; `for_job`
  and `pack`; `render` and `create`; wait for `stage` to run, writing
  `[worker]` lines to the build log as the pod's status changes; hand in;
  follow the log; wait for the build container to end, or the pod to fail
  or vanish, or the stop; hand back and relay; map; **delete the pod on
  every path out**, including errors and panics of this task's own making
  (do the deleting in one place, around the rest).
- A stop deletes the pod with a 5 s grace and reports through
  `outcome::complete(Some(stop), ..)`.
- `transfer.rs`: hand-in (exec `receive`, write the tar, require
  `Succeeded`); the log follower (exec `tail --from <offset>`, pump into
  `BuildLog`, start again from the last byte forwarded when the stream is
  `Lost`, stop when the build is over and the stream ended); hand-back (exec
  `manifest`, bounded read, parse); relay (per artifact: exec `send`, wrap
  stdout in a reader that counts and hashes, `upload_artifact` with the
  manifest's size, then require the upload accepted, the exec `Succeeded`,
  and the count and digest equal; one retry from a fresh exec unless the
  error is `PackagesRefused`). The dispatcher re-checks each manifest name
  with `is_artifact` before using it in a URL. The upload goes through a
  small trait the tests can replace, with `WorkerClient` as its only real
  implementation.
- `outcome.rs`: the table in "How a build ended", as a pure function from
  the pod's status and the hand-back to an `Ending` or a finished report.
- `ready_for_work`: no pod of this dispatcher unschedulable, and `admits`
  of a representative pod is not `Refused` (log the message when it
  changes; `Unknown` counts as ready).
- `reconfigure` takes every declared value for the next build.
- Nothing here holds a package: no temporary file, no `Vec` of a file's
  bytes.

**Tests.**

- `outcome.rs`: every row of the table, from hand-written pod statuses.
- `tests/support/`: a `FakeCluster` implementing `Cluster` in memory: pods
  whose status the test advances, `exec` answered by closures over
  in-memory pipes, calls recorded.
- `tests/lifecycle.rs`, against it and a recording upload sink: a build
  that succeeds with two packages (each upload saw the manifest's size and
  bytes; the pod was deleted; the report carries the measurements); a
  cancel while pending and while building; a pod unschedulable past the
  timeout; `ErrImagePull`; `OOMKilled` with no report; an eviction for its
  own limit and one for the node's; the pod vanishing; a log stream `Lost`
  twice and resumed without a repeated or missing byte; a digest mismatch
  retried once and then `Interrupted`; a `PackagesRefused` not retried; a
  manifest naming `../x` refused; a creation `Refused`; `ready_for_work`
  false while a pod is unschedulable and while admission refuses.
- `tests/streaming.rs`: the real worker listener in-process, as
  `aurcache-worker/tests/fake_worker.rs` starts it, a `KubeExecutor` over
  the `FakeCluster` whose `send` yields a crafted package in small chunks:
  the build completes and the package is published. And a 256 MiB stream of
  zeros relayed with the process's peak resident memory checked to have
  grown by less than 32 MiB.

**Verify.** `cd backend && cargo clippy -p aurcache-worker-kube --all-targets -- -D warnings && cargo test -p aurcache-worker-kube`

**Accepts when** those pass and no test sleeps on real time where paused
time or an event would do.

### WP10. Images, packaging, CI

**Goal.** The agent reaches every place it runs from, the dispatcher has an
image, and CI builds and tests the new parts. Section 10 has the reasoning.

**Owns.** `docker/kube-worker.Dockerfile`, `docker/build-kube-worker.sh`
(new); `scripts/build-images.sh`; `packaging/common.sh`,
`packaging/install-files.sh`; `.github/workflows/ci.yml`,
`.github/workflows/docker-worker-publish.yml`; `Justfile`.

**Does.**

- `packaging/common.sh`: `_aurcache_build_agent`, which builds the agent
  statically for `$CARCH` (section 4.7's command, with the cross setup
  `_aurcache_setup_cross` already does, in a target directory of its own so
  the differing flags do not rebuild the worker) and copies it into
  `$(_aurcache_release_dir)`; `_aurcache_cargo_build` calls it when the
  packages it was asked for include `aurcache-worker-docker`. The PKGBUILD
  in the submodule is therefore unchanged.
- `packaging/install-files.sh`, role `worker-docker`: installs the agent at
  `/usr/lib/aurcache/bin/aurcache-build-agent`; the comment there describes
  the Docker worker as it is.
- `docker/kube-worker.Dockerfile`: the builder stage cross-compiles from
  amd64 as `docker/server.Dockerfile` does (through
  `docker/build-kube-worker.sh`, which takes the triples from
  `packaging/common.sh`): `aurcache-worker-kube` dynamically, the agent
  statically (Debian's `libc6-dev-<arch>-cross` has the static libc). The
  runtime stage is `debian:bookworm-slim`, runs as a non-root uid, with
  `WORKER_DATA_DIR=/var/lib/aurcache-worker` as a volume.
- `scripts/build-images.sh`: `kube-worker` → `aurcache-worker-kube`, for the
  default platforms; documented in its header and `--help`.
- `docker-worker-publish.yml`: a step publishing it, with the others.
- `ci.yml`: "Check package build shapes" gains
  `cargo check -p aurcache-worker-kube` and
  `cargo check -p aurcache-build-agent`; a job "Build agent test" runs
  `./scripts/test-build-agent.sh` on every push, beside the sandbox test.
- `Justfile`: `test-agent` for the script.

**Verify.**

```bash
shellcheck packaging/common.sh packaging/install-files.sh docker/build-kube-worker.sh
./scripts/build-images.sh --images kube-worker --platforms linux/amd64 --tag wt-k8s-wp10 localhost/wt-k8s-wp10
docker run --rm --entrypoint /usr/local/bin/aurcache-build-agent localhost/wt-k8s-wp10/aurcache-worker-kube:wt-k8s-wp10 --interface
./scripts/build-images.sh --images kube-worker --platforms linux/arm64 --tag wt-k8s-wp10 localhost/wt-k8s-wp10   # cross: builds; run it if qemu is registered
# the package, which needs the AUR submodules (git submodule update --init):
./scripts/build-packages.sh --packages aurcache-worker-docker
```

and check the package's file list has the agent, statically linked. Building
the hybrid image itself (twenty minutes or more) is WP14's.

### WP11. Documentation: the Kubernetes worker page

**Goal.** `docs/docs/workers/kubernetes.md`: what an operator needs to run
the Kubernetes worker, and to decide whether to.

**Owns.** `docs/docs/workers/kubernetes.md` (new; `sidebar_position`
between the storage pool's and routing's is taken, so use a fractional or
renumber nothing: Docusaurus accepts `3.5`).

**Covers.** What it is and when to choose it over the chroot worker on
Kubernetes (no privileged pod, no kernel requirement; no caches, no
persistent build directory, no kept builds, a disk limit that evicts);
what the cluster needs (a namespace at `baseline`, RBAC, a CNI that
enforces NetworkPolicy, one small volume); installing from
`kubernetes/kube-worker/`; approving it; every variable of section 4.6, in
the style of `split-chroot.md`'s tables; limits and what "best effort"
means here; architectures and the pod template; the SSH credential Secret;
user namespaces and their requirements; what a build pod is made of and
what it can reach; one replica only, and why; troubleshooting (a pod that
never schedules, an image that cannot be pulled, an admission refusal, the
probe's warning). Written from the design document, checked against the
code WP4 and WP9 merged.

**Verify.** `cd docs && yarn install --frozen-lockfile && yarn build` with
no broken link.

### WP12. Kubernetes: the NetworkPolicy probe

**Goal.** At startup, find out whether the builds' NetworkPolicy is
enforced, and say so loudly when it is not.

**Owns.** `aurcache-worker-kube/src/probe.rs`.

**Notes.** A pod with the build pods' labels (so the policy selects it),
one container from the agent image running
`probe --timeout 5 --connect <worker port> --connect 169.254.169.254:80`
and, when there is a repository, `--connect <repository>`; `restricted`
security context; `activeDeadlineSeconds` of a minute. Its output is read
with `log_tail`. `Open` when the worker port or the metadata address
answered; `Enforced` when neither did (the repository not answering is
logged as a separate warning: builds will not find AURCache's own packages);
`Inconclusive` when the pod could not be created, did not finish, or
printed nothing readable. The pod is deleted on every path.

**Tests.** Against a small in-file fake of `Cluster`: each outcome; the pod
carries the labels the policy selects and is deleted.

**Verify.** `cd backend && cargo clippy -p aurcache-worker-kube --all-targets -- -D warnings && cargo test -p aurcache-worker-kube --lib probe`

This is the work package to drop if the branch runs long: `main.rs` then
skips the call, and the documentation's sentence about it goes.

### WP13. The kind suite

**Goal.** `scripts/test-e2e-kube.sh`: the Kubernetes worker, from the
shipped manifests, building a package in a real cluster.

**Owns.** `scripts/test-e2e-kube.sh`, `scripts/fixtures/kube-e2e/` (new),
`docker/testpkg/big/` (new), `.github/workflows/e2e-kube.yml` (new).

**Does.** Needs `kind`, `kubectl`, `docker`, `jq`; fails at once, saying
which is missing. Creates a cluster named from `AURCACHE_TEST_PREFIX`;
builds the server, `aurcache-worker-kube` and builder images for the host's
platform and loads them (`kind load docker-image`); applies a kustomization
under `scripts/fixtures/kube-e2e/` that takes `kubernetes/kube-worker/` as
its base and adds a minimal server (a Deployment and the `aurcache` Service
in namespace `aurcache`, with the labels the NetworkPolicy selects), the
local image names, `WORKER_KUBE_IMAGE_PULL_POLICY=IfNotPresent` and an
enrollment token. Then, driving the server with `aurcli` as
`scripts/lib/e2e.sh` does:

1. the worker enrolls, is approved, and shows as `kubernetes`;
2. the package builds (`hello` from the AUR, as in the other suites); its
   log has the build's output; the build reports a peak memory figure and
   disk use; the package installs from the repository;
3. a pod with the build labels cannot open the server's worker port (when
   the cluster's CNI enforces policies, which kind's does; otherwise the
   probe's warning is asserted instead);
4. a build is cancelled, and its pod is gone within a minute;
5. the dispatcher is deleted while a build runs; the new one sweeps the
   pod, and the server queues the build again;
6. `docker/testpkg/big` (a package of 2 GiB of random bytes, served from a
   git daemon in the cluster, as `test-e2e-ssh.sh` serves its fixture)
   builds and publishes; the script prints the relay's throughput and the
   dispatcher pod's peak memory, and fails if that memory passed 256 MiB;
7. with `E2E_USER_NAMESPACES=1`, leg 2 again with
   `WORKER_KUBE_USER_NAMESPACES=true`, skipped with a message when the node
   refuses the pod.

The cleanup follows `scripts/lib/e2e.sh`'s policy (`CLEANUP`), and deletes
the cluster by name. `e2e-kube.yml` runs it nightly when something landed
that day, on `workflow_dispatch`, and on a pull request labelled
`run-e2e-kube`, installing a pinned `kind` release by checksum into the
runner's temporary directory.

**Verify.** `KIND=<path> AURCACHE_TEST_PREFIX=wt-k8s-wp13 ./scripts/test-e2e-kube.sh hello`

Needs the answer to question 1 of section 11. Without a cluster, the script
and the workflow are written and reviewed but not run, and the hand-back
says so in its first line.

### WP14. Integration

**Goal.** One branch that passes everything, with the seams closed.

**Owns.** Everything, for fixes; `AGENTS.md` ("Build, test, and lint
commands" and "Which end-to-end suite to run" gain `test-build-agent.sh` and
`test-e2e-kube.sh`, with when to run each); the status lines of the three
design documents.

**Does**, in order, fixing what fails and noting each fix:

```bash
git submodule update --init
just lint
just test
./scripts/test-build-agent.sh
./scripts/test-frontend.sh
./scripts/test-sandbox.sh
./scripts/test-kernel.sh                      # the chroot worker now calls the two moved functions
cd docs && yarn install --frozen-lockfile && yarn build
./scripts/test-e2e-hybrid.sh hello            # the Docker worker, on the image's own Podman
./scripts/test-e2e-hybrid-legacy.sh hello     # the Docker worker, on the host's daemon
./scripts/test-e2e.sh hello                   # the chroot worker: unchanged, and still so
./scripts/test-e2e-kube.sh hello              # when a cluster is allowed
```

The hybrid suites now expect measurements: `E2E_EXPECT_DISK_USAGE` is no
longer set to 0 in `test-e2e-hybrid-legacy.sh`. That script shares a
directory under `/tmp` with the daemon by default; point it elsewhere on a
machine where `/tmp` is small.

Then: read the whole diff once for the code-style rules of section 5 (above
all comments that tell history), for user-facing text that still says
"legacy", and for anything that holds a package in memory or on the
dispatcher's disk; check that `git diff upstream/master --stat --
backend/aurcache-common/src/worker.rs
backend/aurcache-common/tests/fixtures/worker-protocol` is empty; update the
design documents where the code ended up different, and move nothing to
`implemented/` (that is for after the merge).

---

## 7. Waves

```
wave 1   WP0 ─────────────────────────────────────────────────────────────┐
                                                                           │
wave 2   WP1 agent: build        WP2 agent: transfer   WP3 worker side     │
         WP4 kube: render        WP5 kube: cluster     WP6 names and docs  │
         WP7 manifests                                                     │
              │  │                    │   │                 │              │
wave 3   WP8 docker (1,3)   WP9 kube executor (2,3,4,5)   WP10 images (1,2)
         WP11 kube docs (4,7)        WP12 probe (2,4,5)
              │                           │                   │
wave 4   WP13 kind suite  ───────────►  WP14 integration
```

- **Wave 1** is one work package and must be merged before anything else
  starts: it is what makes the rest parallel.
- **Wave 2** is seven work packages with no file in common. WP6 and WP7
  depend on nothing in the code and can start with wave 1.
- **Wave 3** starts from the branch with all of wave 2 merged. Five work
  packages, no file in common.
- **Wave 4** is sequential: WP13, then WP14. If no cluster is allowed, WP13
  shrinks to writing the script, and can run beside wave 3.

Seven concurrent Rust builds are more than the machine's target disk takes
comfortably. Run at most four work packages that compile at a time (in wave
2: WP1, WP2, WP3, WP5 first; WP4 when one finishes; WP6 and WP7 compile
little or nothing).

---

## 8. Integration

Each work package commits on `k8s/<wp>`, in its own worktree, branched from
`feat/kubernetes-executor` as it stands when its wave starts. The
orchestrator merges, with `git merge --no-ff`, into
`feat/kubernetes-executor`:

| after wave | merge order | then check |
|---|---|---|
| 1 | WP0 | `cd backend && cargo clippy --all-targets -- -D warnings` |
| 2 | any `deps:` branch first; then WP3, WP1, WP2, WP4, WP5, WP6, WP7 | `cd backend && cargo check --workspace --all-targets` |
| 3 | any `deps:` branch first; then WP8, WP9, WP12, WP10, WP11 | `cd backend && cargo check --workspace --all-targets && cargo test -p aurcache-build-agent -p aurcache-worker-container -p aurcache-worker-docker -p aurcache-worker-kube` |
| 4 | WP13, then WP14 works on the branch itself | WP14's list |

The checks between waves use
`CARGO_TARGET_DIR=/mnt/cargo-target/aurcache/wt-k8s-int/backend`, which
WP14 then inherits.

Within a wave the work packages own disjoint files, so merges do not
conflict; the order above only puts the most depended-on first, so a
failing check points at one branch. A conflict means someone left their
files: resolve it in favour of the owner, and send the other change back.

A work package that reports a needed change to section 4 stops its wave's
dependants: the orchestrator decides, updates section 4 in a commit on
`feat/kubernetes-executor`, and tells the work packages that compile
against it.

---

## 9. Test strategy

| layer | what | where | runs |
|---|---|---|---|
| unit | the helper's validation, the `pacman.conf` check, `makepkg.conf`, pod rendering, the two outcome mappings, settings tables | in each crate | `cargo test`, every push |
| interface | the spec, the report and the hand-back as JSON fixtures | `aurcache-build-agent/tests/interface.rs` | every push |
| the agent's exchanges | each transfer subcommand, as a process | `aurcache-build-agent/tests/transfer.rs` | every push |
| the agent in a container | a real build and a hostile one under the pod's exact flags | `scripts/test-build-agent.sh` | every push (Docker only, a few minutes) |
| the lifecycle | the executor against a fake cluster | `aurcache-worker-kube/tests/lifecycle.rs` | every push |
| streaming | the executor, a fake cluster and the real worker listener; bounded memory | `aurcache-worker-kube/tests/streaming.rs` | every push |
| the seam | `KubeCluster` against a real API server | `#[ignore]` tests in `kube_cluster.rs` | by hand, with `AURCACHE_KUBE_TESTS=1` |
| the Docker worker | the hybrid image, both modes | `test-e2e-hybrid.sh`, `test-e2e-hybrid-legacy.sh` | when the Docker worker, the agent or the hybrid image changes |
| the Kubernetes worker | kind, from the shipped manifests | `scripts/test-e2e-kube.sh` | nightly, and on the `run-e2e-kube` label |

**A cluster for tests.** Neither `kind` nor `k3d` is installed, and nobody
working on this installs system packages. `kubectl` is installed.

- **kind** is one static binary of about 10 MB, released on GitHub with
  checksums. It needs no root: it starts its "node" as a privileged
  container on the Docker daemon already here, the same trust
  `test-e2e.sh` already asks for with its privileged worker. It pulls a
  node image of about 1 GB, and the suite loads three images into it, so
  it costs several gigabytes of Docker's storage until the cluster is
  deleted. Proposed: download a pinned release into
  `/mnt/cargo-target/aurcache/tools/kind/`, verify its checksum, and use it
  by path (`KIND=...`). Nothing is installed, and removing that directory
  and the cluster undoes it.
- **k3s** runs as root, or rootless with delegation set up by hand; its
  wrapper **k3d** is kind's shape with a different node. No advantage here.
- **Without either**, the dispatcher is tested against the fake cluster
  only, and `kube_cluster.rs` (the one file that talks to a real API
  server) is first exercised by CI on the pull request. That is a real gap:
  exec over a WebSocket, back-pressure and the watch are exactly what a
  fake cannot show.

This is the orchestrator's first question to the user (section 11).

---

## 10. Images and packaging

**The agent's three routes.**

| where it runs | how it gets there | who builds it |
|---|---|---|
| a Docker worker's build container | the worker copies its own agent into the build directory | the `aurcache-worker-docker` package ships it at `/usr/lib/aurcache/bin/aurcache-build-agent`: `packaging/common.sh` builds it, `packaging/install-files.sh` installs it. The hybrid image installs that package, so it has it |
| a build pod | the `stage` init container copies it from the agent image into a volume | `docker/kube-worker.Dockerfile` puts it in the dispatcher's image |
| tests | a path (`WORKER_BUILD_AGENT`), or a bind mount | the test script |

**The builder image does not change.** `docker/builder.Dockerfile` and
`docker/builder-entrypoint.sh` are untouched: a released 0.6.0 Docker worker
still starts that image through its entrypoint and `sudo`.

**The AUR submodules do not change**, and cannot yet: a PKGBUILD there
builds a released tag, and the released tag has no agent. What makes the
images work in the meantime is that `packaging/common.sh` and
`packaging/install-files.sh` are in this tree and are what the PKGBUILD
calls. For the next release (a note, not a work package):

- `packaging/aurcache-worker-docker`: `pkgdesc` and the header comment stop
  saying legacy and deprecated; nothing else is needed.
- a new AUR repository `aurcache-worker-kube`, added as a submodule, for
  running the dispatcher outside a cluster's own image. It needs an
  `install-files.sh` role and nothing unusual. Until it exists, the
  dispatcher is distributed as an image only, built without a PKGBUILD, as
  the server's and the demo worker's images are.
- the release workflow's tag patterns gain `aurcache-worker-kube-v*`.

**The dispatcher's image** is Debian-based like the server's: it runs no
pacman and needs no Arch. It must exist for every architecture builds run
on, since `stage` and `hold` run on the build's node: amd64, arm64 and
arm/v7, the platforms the other images have.

**The manifests** live in `kubernetes/kube-worker/`, a kustomization of its
own, applied after the server's:

```bash
kubectl apply -k kubernetes/                # the server (and the chroot workers, unless removed)
kubectl apply -k kubernetes/kube-worker/    # the Kubernetes worker
```

It cannot be a component of the root kustomization that the branch
`docs/199-kubernetes` adds: that one sets `namespace: aurcache` for
everything it assembles, which would move the builds' Role and
NetworkPolicy into the server's namespace and rename the builds' Namespace
object itself. Its resources name their namespaces instead.

**Merge order with `docs/199-kubernetes`.** That branch creates
`kubernetes/` (README, root kustomization, server, chroot workers) and
`docs/docs/setup/kubernetes.md`. This one adds only new files under
`kubernetes/kube-worker/` and refers to the server by the names that branch
gives it (Service `aurcache`, namespace `aurcache`, the server pod's labels,
ports 8081 and 8083). So neither order conflicts. Whichever merges second
adds the cross-references: a row for `kube-worker/` in
`kubernetes/README.md`, and a paragraph in `docs/docs/setup/kubernetes.md`
pointing at the Kubernetes worker's page as the unprivileged alternative.
Until `docs/199-kubernetes` is merged, the kind suite brings its own
minimal server manifest.

---

## 11. Risks, and where to stop and ask

### Questions for the user

Before wave 2:

1. **May the work download `kind`** (a pinned release, checksum verified)
   into `/mnt/cargo-target/aurcache/tools/kind/` and run throwaway clusters
   on the local Docker daemon? Without it, the part that talks to a real
   API server is first run by CI on the pull request (section 9).
2. **The Docker worker's page moves** from `/docs/workers/legacy-docker` to
   `/docs/workers/docker`, which adds `@docusaurus/plugin-client-redirects`
   to the docs' dependencies so the old address keeps working. Fine, or keep
   the old file name?
3. **The hybrid image**: this plan stops calling the *worker* in it
   deprecated and keeps describing the *image* as the compatibility image
   for compose files from before the split. Or is the image supported again
   too?
4. **The agent is brought by the worker, not baked into
   `aurcache-builder-v2`** (section 2). The brief assumed the builder image.
   Confirm.
5. **Behaviour changes for Docker worker users on upgrade**: build code
   loses `sudo`; `makepkg -i` and `-r` in build flags stop working; a
   failed `pacman -Syu` or key import is reported as the worker's problem
   and the build is queued again while it has attempts, where it failed
   the build before. Acceptable in a minor release, with a line in the
   upgrade notes?
6. **Kubernetes limits default to unset, and a set limit is also the
   request** (a build told it has 16 CPUs waits for a node with 16 free).
   Confirm, or default the disk limit as the chroot worker does (50 GiB)?

Any time before wave 4:

7. **Scope**: is a standalone image and compose file for the Docker worker
   wanted in this pull request? The plan says no: it stays in the hybrid
   image and the native package.
8. **The NetworkPolicy probe** (WP12) is the part to cut if the branch runs
   long. Keep it in?

### Stop and ask during the work

A work package stops and reports, and the orchestrator asks the user, when:

- anything would need a change to `aurcache-common/src/worker.rs`, to a
  protocol fixture, or to the server;
- a build needs a capability beyond the seven (`SYS_CHROOT`, `MKNOD`,
  `SETFCAP`): add none without asking;
- the pacman helper refuses something makepkg legitimately sends: the
  grammar is widened deliberately, never to make a test pass;
- the static glibc agent does not build or run for some architecture or
  image (the fallback is a musl target, which changes the packaging);
- `kube` 5.1 cannot stream an exec's stdout with back-pressure, or cannot
  be made to share the workspace's rustls provider;
- the hybrid image's Podman refuses the new container options;
- quota is not evaluated in a server-side dry run (the readiness check then
  needs another way);
- the shared target disk has less than 20 GB free.

### Risks

| risk | what limits it |
|---|---|
| Throughput of a large package through the API server | measured in the kind suite; the future direct upload is designed; the relay holds nothing in memory or on disk either way |
| `kube` 5.1's API differs from what section 4.5 assumes | WP0 writes `cluster.rs` against the real crate and corrects the section; WP5 checks docs.rs |
| The seam is only tested against a fake until WP13 | the `#[ignore]` tests in WP5, if question 1 is a yes |
| Seven work packages compiling against stubs produce seams | interfaces as code in WP0; a workspace check after every wave; WP14 |
| The Docker worker's upgrade changes behaviour for 0.6.0 deployments | both hybrid suites; question 5; the upgrade notes |
| A dependency's install script needs a capability the build container lacks | fails loudly, in that package only; the three candidates are on `baseline`'s list |
| The image builds are slow to iterate on (the hybrid image above all) | WP10 verifies the package and the dispatcher image, not the hybrid image; WP14 builds that once |
| `docs/199-kubernetes` changes the names this refers to | WP7 reads that branch; only names, in two files |
| Disk for parallel builds | per-crate builds, debug info off, four compiling at once, target directories removed on merge |

---

## 12. After this pull request

In `kubernetes-executor.md` ("How it is being built", the list left for
later) and `container-workers.md` ("Later"). The first three worth doing:
source caches on `ReadWriteOncePod` volumes, the direct upload from the pod
if the kind measurement says the relay is slow, and the AUR package for the
dispatcher at the release that first contains it.
