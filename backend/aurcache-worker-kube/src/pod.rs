//! A build pod, rendered from the configuration and a job, with nothing in
//! between that needs a cluster.
//!
//! `kubernetes-executor.md` ("The build pod") is what this renders, field for
//! field.

use crate::config::{Config, PodExtras};
use anyhow::{Result, anyhow};
use aurcache_build_agent::layout::{self, Work};
use aurcache_common::worker::JobDescriptor;
use aurcache_worker_container::arch;
use k8s_openapi::api::core::v1::{
    Capabilities, Container, EmptyDirVolumeSource, Pod, PodSecurityContext, PodSpec,
    ResourceRequirements, SeccompProfile, SecretVolumeSource, SecurityContext, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

/// `app.kubernetes.io/managed-by` on every pod this worker creates.
pub const MANAGED_BY: &str = "aurcache-worker-kube";
/// The container that runs the build.
pub const BUILD: &str = "build";
/// The init container that installs the agent and receives the job.
pub const STAGE: &str = "stage";
/// The container that keeps the pod up for the hand-back.
pub const HOLD: &str = "hold";
/// Where the work volume is mounted, in every container.
pub const WORK: &str = "/work";
/// The ServiceAccount build pods run as: bound to nothing, token never mounted.
pub const SERVICE_ACCOUNT: &str = "aurcache-build";

const TOOLS: &str = "/aurcache/bin";
const CREDENTIALS: &str = "/aurcache/credentials";
/// Where the agent lives in the dispatcher's image.
const AGENT_IN_IMAGE: &str = "/usr/local/bin";
/// The user `stage` and `hold` run as.
const NOBODY: i64 = 65534;
/// A build pod outlives its build's timeout by this long, then the kubelet
/// ends it, whether or not a dispatcher is there to. The timeout bounds the
/// build and not the hand-back, so this is also how long the packages of a
/// build that ended at its timeout have to reach the server.
const DEADLINE_GRACE: Duration = Duration::from_secs(60 * 60);
/// A Secret's files are readable by their owner alone.
const SECRET_MODE: i32 = 0o400;

const LABEL_MANAGED_BY: &str = "app.kubernetes.io/managed-by";
const LABEL_DISPATCHER: &str = "aurcache.io/dispatcher";
const LABEL_BUILD: &str = "aurcache.io/build-id";
const LABEL_ARCH: &str = "aurcache.io/arch";
const NODE_OS: &str = "kubernetes.io/os";
const NODE_ARCH: &str = "kubernetes.io/arch";

/// What a build pod is rendered from.
pub struct PodInputs<'a> {
    pub cfg: &'a Config,
    pub agent_image: &'a str,
    pub job: &'a JobDescriptor,
    pub timeout: Option<Duration>,
}

/// `aurcache-build-<dispatcher>-<build_id>`.
#[must_use]
pub fn name(dispatcher: &str, build_id: i32) -> String {
    format!("aurcache-build-{dispatcher}-{build_id}")
}

/// `aurcache.io/dispatcher=<dispatcher>`.
#[must_use]
pub fn selector(dispatcher: &str) -> String {
    format!("{LABEL_DISPATCHER}={dispatcher}")
}

/// Labels every pod of this dispatcher carries, a build's or not.
#[must_use]
pub fn labels(dispatcher: &str) -> BTreeMap<String, String> {
    BTreeMap::from([
        (LABEL_MANAGED_BY.to_string(), MANAGED_BY.to_string()),
        (LABEL_DISPATCHER.to_string(), dispatcher.to_string()),
    ])
}

/// The build a pod of this worker's was made for; `None` for a pod that is
/// no build's.
#[must_use]
pub fn build_id(pod: &Pod) -> Option<i32> {
    pod.metadata.labels.as_ref()?.get(LABEL_BUILD)?.parse().ok()
}

fn quantity(text: impl Into<String>) -> Quantity {
    Quantity(text.into())
}

/// CPUs as a quantity, in milli-CPUs rounded up.
fn cpu_quantity(cpus: f64) -> Quantity {
    // Saturating on purpose: a limit past `u64` is as good as none.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let millis = (cpus * 1000.0).ceil() as u64;
    quantity(format!("{millis}m"))
}

/// Requests and limits that say the same thing; `None` when no line is set.
fn resources(cfg: &Config) -> Option<ResourceRequirements> {
    let mut lines = BTreeMap::new();
    if let Some(cpus) = cfg.tunables.cpus {
        lines.insert("cpu".to_string(), cpu_quantity(cpus));
    }
    if let Some(bytes) = cfg.tunables.memory {
        lines.insert("memory".to_string(), quantity(bytes.to_string()));
    }
    if let Some(bytes) = cfg.tunables.disk {
        lines.insert("ephemeral-storage".to_string(), quantity(bytes.to_string()));
    }
    if lines.is_empty() {
        return None;
    }
    Some(ResourceRequirements {
        requests: Some(lines.clone()),
        limits: Some(lines),
        ..ResourceRequirements::default()
    })
}

/// What `stage` and `hold` run as: nobody, with nothing.
fn unprivileged() -> SecurityContext {
    SecurityContext {
        run_as_user: Some(NOBODY),
        run_as_group: Some(NOBODY),
        run_as_non_root: Some(true),
        allow_privilege_escalation: Some(false),
        read_only_root_filesystem: Some(true),
        capabilities: Some(Capabilities {
            drop: Some(vec!["ALL".to_string()]),
            add: None,
        }),
        ..SecurityContext::default()
    }
}

/// What `build` runs as: root, with the seven capabilities the root step
/// needs and no way to gain more.
fn build_security() -> SecurityContext {
    SecurityContext {
        run_as_user: Some(0),
        allow_privilege_escalation: Some(false),
        capabilities: Some(Capabilities {
            drop: Some(vec!["ALL".to_string()]),
            add: Some(layout::CAPABILITIES.map(String::from).to_vec()),
        }),
        ..SecurityContext::default()
    }
}

fn mount(name: &str, path: &str, read_only: bool) -> VolumeMount {
    VolumeMount {
        name: name.to_string(),
        mount_path: path.to_string(),
        read_only: read_only.then_some(true),
        ..VolumeMount::default()
    }
}

/// The agent, where a container of the dispatcher's image has it: what
/// `stage` and `hold` run, and what an exec into either names.
#[must_use]
pub fn agent_path() -> String {
    format!("{AGENT_IN_IMAGE}/{}", layout::BIN_NAME)
}

/// Where the build container has the files of the credential Secret; `None`
/// when the configuration names no Secret, and the pod mounts none.
#[must_use]
pub fn credentials(cfg: &Config) -> Option<&'static Path> {
    cfg.credential_secret
        .as_ref()
        .map(|_| Path::new(CREDENTIALS))
}

fn stage(inputs: &PodInputs<'_>) -> Container {
    Container {
        name: STAGE.to_string(),
        image: Some(inputs.agent_image.to_string()),
        image_pull_policy: Some(inputs.cfg.pull_policy.clone()),
        command: Some(
            [&agent_path(), "stage", "--work", WORK, "--tools", TOOLS]
                .map(String::from)
                .to_vec(),
        ),
        security_context: Some(unprivileged()),
        volume_mounts: Some(vec![
            mount("work", WORK, false),
            mount("tools", TOOLS, false),
        ]),
        ..Container::default()
    }
}

fn build(inputs: &PodInputs<'_>) -> Container {
    let log = Work::new(WORK).log();
    let mut mounts = vec![mount("work", WORK, false), mount("tools", TOOLS, true)];
    if let Some(path) = credentials(inputs.cfg) {
        mounts.push(mount("credentials", &path.to_string_lossy(), true));
    }
    Container {
        name: BUILD.to_string(),
        image: Some(inputs.cfg.builder_image.clone()),
        image_pull_policy: Some(inputs.cfg.pull_policy.clone()),
        command: Some(vec![
            format!("{TOOLS}/{}", layout::BIN_NAME),
            "build".to_string(),
            "--work".to_string(),
            WORK.to_string(),
            "--log-file".to_string(),
            log.display().to_string(),
        ]),
        security_context: Some(build_security()),
        resources: resources(inputs.cfg),
        volume_mounts: Some(mounts),
        ..Container::default()
    }
}

fn hold(inputs: &PodInputs<'_>) -> Container {
    let small = |text: &str| quantity(text);
    Container {
        name: HOLD.to_string(),
        image: Some(inputs.agent_image.to_string()),
        image_pull_policy: Some(inputs.cfg.pull_policy.clone()),
        command: Some(vec![agent_path(), "hold".to_string()]),
        security_context: Some(unprivileged()),
        resources: Some(ResourceRequirements {
            requests: Some(BTreeMap::from([
                ("cpu".to_string(), small("10m")),
                ("memory".to_string(), small("16Mi")),
            ])),
            limits: Some(BTreeMap::from([("memory".to_string(), small("64Mi"))])),
            ..ResourceRequirements::default()
        }),
        volume_mounts: Some(vec![mount("work", WORK, true)]),
        ..Container::default()
    }
}

fn volumes(cfg: &Config) -> Vec<Volume> {
    let mut all = vec![
        Volume {
            name: "work".to_string(),
            empty_dir: Some(EmptyDirVolumeSource {
                size_limit: cfg.tunables.disk.map(|bytes| quantity(bytes.to_string())),
                ..EmptyDirVolumeSource::default()
            }),
            ..Volume::default()
        },
        Volume {
            name: "tools".to_string(),
            empty_dir: Some(EmptyDirVolumeSource {
                size_limit: Some(quantity("64Mi")),
                ..EmptyDirVolumeSource::default()
            }),
            ..Volume::default()
        },
    ];
    if let Some(secret) = &cfg.credential_secret {
        all.push(Volume {
            name: "credentials".to_string(),
            secret: Some(SecretVolumeSource {
                secret_name: Some(secret.clone()),
                default_mode: Some(SECRET_MODE),
                ..SecretVolumeSource::default()
            }),
            ..Volume::default()
        });
    }
    all
}

/// The pod's labels: the operator's under this worker's.
fn pod_labels(inputs: &PodInputs<'_>, arch: &str) -> BTreeMap<String, String> {
    let mut all = inputs.cfg.extras.labels.clone().unwrap_or_default();
    all.extend(labels(&inputs.cfg.dispatcher));
    all.insert(LABEL_BUILD.to_string(), inputs.job.build_id.to_string());
    all.insert(LABEL_ARCH.to_string(), arch.to_string());
    all
}

/// The node selector: the operator's under the OS and architecture.
fn node_selector(extras: &PodExtras, oci_arch: &str) -> BTreeMap<String, String> {
    let mut all = extras.node_selector.clone().unwrap_or_default();
    all.insert(NODE_OS.to_string(), "linux".to_string());
    all.insert(NODE_ARCH.to_string(), oci_arch.to_string());
    all
}

fn deadline(timeout: Option<Duration>) -> Option<i64> {
    let total = timeout?.saturating_add(DEADLINE_GRACE);
    Some(i64::try_from(total.as_secs()).unwrap_or(i64::MAX))
}

/// The pod for one build. An error for an architecture with no OCI name.
pub fn render(inputs: &PodInputs<'_>) -> Result<Pod> {
    let job_arch = &inputs.job.arch;
    let platform = arch::platform(job_arch)
        .ok_or_else(|| anyhow!("architecture {job_arch:?} has no Kubernetes name"))?;
    let cfg = inputs.cfg;
    let extras = &cfg.extras;
    Ok(Pod {
        metadata: ObjectMeta {
            name: Some(name(&cfg.dispatcher, inputs.job.build_id)),
            namespace: Some(cfg.namespace.clone()),
            labels: Some(pod_labels(inputs, job_arch)),
            annotations: extras.annotations.clone(),
            ..ObjectMeta::default()
        },
        spec: Some(PodSpec {
            restart_policy: Some("Never".to_string()),
            active_deadline_seconds: deadline(inputs.timeout),
            service_account_name: Some(SERVICE_ACCOUNT.to_string()),
            automount_service_account_token: Some(false),
            enable_service_links: Some(false),
            host_users: cfg.user_namespaces.then_some(false),
            node_selector: Some(node_selector(extras, platform.arch)),
            security_context: Some(PodSecurityContext {
                seccomp_profile: Some(SeccompProfile {
                    type_: "RuntimeDefault".to_string(),
                    localhost_profile: None,
                }),
                ..PodSecurityContext::default()
            }),
            init_containers: Some(vec![stage(inputs)]),
            containers: vec![build(inputs), hold(inputs)],
            volumes: Some(volumes(cfg)),
            tolerations: extras.tolerations.clone(),
            affinity: extras.affinity.clone(),
            priority_class_name: extras.priority_class_name.clone(),
            runtime_class_name: extras.runtime_class_name.clone(),
            image_pull_secrets: extras.image_pull_secrets.clone(),
            scheduler_name: extras.scheduler_name.clone(),
            topology_spread_constraints: extras.topology_spread_constraints.clone(),
            ..PodSpec::default()
        }),
        ..Pod::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AgentImage, Tunables};
    use aurcache_common::worker::MirrorlistOffer;
    use serde_json::{Value, json};

    fn config() -> Config {
        Config {
            namespace: "aurcache-builds".into(),
            dispatcher: "abcdef0123456789".into(),
            builder_image: "reg/builder:1".into(),
            pull_policy: "Always".into(),
            agent_image: AgentImage::Given("reg/agent@sha256:1".into()),
            credential_secret: None,
            user_namespaces: false,
            extras: PodExtras::default(),
            network_probe: true,
            tunables: Tunables {
                memory: None,
                cpus: None,
                disk: None,
                schedule_timeout: Duration::from_secs(600),
                keyserver: "hkps://k".into(),
            },
        }
    }

    fn job(build_id: i32, arch: &str) -> JobDescriptor {
        JobDescriptor {
            build_id,
            pkgbase: "hello".into(),
            arch: arch.into(),
            build_flags: vec![],
            persistent_builddir: false,
            makepkg_conf: String::new(),
            pacman_conf: String::new(),
            mirrorlist: MirrorlistOffer::Nothing,
            pgp_keys: vec![],
            vcs_sources: vec![],
            expected_packages: vec!["hello".into()],
            timeout_secs: None,
            expected_version: "1-1".into(),
        }
    }

    fn render_with(cfg: &Config, build_id: i32, arch: &str, timeout: Option<u64>) -> Result<Pod> {
        render(&PodInputs {
            cfg,
            agent_image: "reg/agent@sha256:1",
            job: &job(build_id, arch),
            timeout: timeout.map(Duration::from_secs),
        })
    }

    fn pod(cfg: &Config) -> Pod {
        render_with(cfg, 7, "x86_64", Some(3600)).unwrap()
    }

    fn spec(pod: &Pod) -> &PodSpec {
        pod.spec.as_ref().unwrap()
    }

    fn container<'a>(pod: &'a Pod, which: &str) -> &'a Container {
        let spec = spec(pod);
        spec.containers
            .iter()
            .chain(spec.init_containers.iter().flatten())
            .find(|c| c.name == which)
            .unwrap()
    }

    fn limited() -> Config {
        let mut cfg = config();
        cfg.tunables.memory = Some(8 << 30);
        cfg.tunables.cpus = Some(1.5);
        cfg.tunables.disk = Some(30 << 30);
        cfg
    }

    #[test]
    fn names_and_selector() {
        assert_eq!(
            name("abcdef0123456789", 7),
            "aurcache-build-abcdef0123456789-7"
        );
        assert_eq!(
            selector("abcdef0123456789"),
            "aurcache.io/dispatcher=abcdef0123456789"
        );
        assert_eq!(labels("d")["app.kubernetes.io/managed-by"], MANAGED_BY);
        assert_eq!(labels("d")["aurcache.io/dispatcher"], "d");
    }

    /// The whole pod, as the API server receives it.
    #[test]
    fn the_whole_pod() {
        let mut cfg = limited();
        cfg.credential_secret = Some("ssh".into());
        cfg.user_namespaces = true;
        let value =
            serde_json::to_value(render_with(&cfg, 7, "aarch64", Some(3600)).unwrap()).unwrap();
        let unprivileged = json!({
            "runAsUser": 65534, "runAsGroup": 65534, "runAsNonRoot": true,
            "allowPrivilegeEscalation": false, "readOnlyRootFilesystem": true,
            "capabilities": {"drop": ["ALL"]},
        });
        let limits = json!({"cpu": "1500m", "memory": "8589934592",
                            "ephemeral-storage": "32212254720"});
        assert_eq!(
            value,
            json!({
                "apiVersion": "v1",
                "kind": "Pod",
                "metadata": {
                    "name": "aurcache-build-abcdef0123456789-7",
                    "namespace": "aurcache-builds",
                    "labels": {
                        "app.kubernetes.io/managed-by": "aurcache-worker-kube",
                        "aurcache.io/dispatcher": "abcdef0123456789",
                        "aurcache.io/build-id": "7",
                        "aurcache.io/arch": "aarch64",
                    },
                },
                "spec": {
                    "restartPolicy": "Never",
                    "activeDeadlineSeconds": 7200,
                    "serviceAccountName": "aurcache-build",
                    "automountServiceAccountToken": false,
                    "enableServiceLinks": false,
                    "hostUsers": false,
                    "nodeSelector": {"kubernetes.io/os": "linux", "kubernetes.io/arch": "arm64"},
                    "securityContext": {"seccompProfile": {"type": "RuntimeDefault"}},
                    "initContainers": [{
                        "name": "stage",
                        "image": "reg/agent@sha256:1",
                        "imagePullPolicy": "Always",
                        "command": ["/usr/local/bin/aurcache-build-agent", "stage",
                                    "--work", "/work", "--tools", "/aurcache/bin"],
                        "securityContext": unprivileged,
                        "volumeMounts": [
                            {"name": "work", "mountPath": "/work"},
                            {"name": "tools", "mountPath": "/aurcache/bin"},
                        ],
                    }],
                    "containers": [
                        {
                            "name": "build",
                            "image": "reg/builder:1",
                            "imagePullPolicy": "Always",
                            "command": ["/aurcache/bin/aurcache-build-agent", "build",
                                        "--work", "/work", "--log-file", "/work/log/build.log"],
                            "securityContext": {
                                "runAsUser": 0,
                                "allowPrivilegeEscalation": false,
                                "capabilities": {
                                    "drop": ["ALL"],
                                    "add": ["CHOWN", "DAC_OVERRIDE", "FOWNER", "FSETID",
                                            "SETUID", "SETGID", "KILL"],
                                },
                            },
                            "resources": {"requests": limits, "limits": limits},
                            "volumeMounts": [
                                {"name": "work", "mountPath": "/work"},
                                {"name": "tools", "mountPath": "/aurcache/bin", "readOnly": true},
                                {"name": "credentials", "mountPath": "/aurcache/credentials",
                                 "readOnly": true},
                            ],
                        },
                        {
                            "name": "hold",
                            "image": "reg/agent@sha256:1",
                            "imagePullPolicy": "Always",
                            "command": ["/usr/local/bin/aurcache-build-agent", "hold"],
                            "securityContext": unprivileged,
                            "resources": {
                                "requests": {"cpu": "10m", "memory": "16Mi"},
                                "limits": {"memory": "64Mi"},
                            },
                            "volumeMounts": [{"name": "work", "mountPath": "/work",
                                              "readOnly": true}],
                        },
                    ],
                    "volumes": [
                        {"name": "work", "emptyDir": {"sizeLimit": "32212254720"}},
                        {"name": "tools", "emptyDir": {"sizeLimit": "64Mi"}},
                        {"name": "credentials",
                         "secret": {"secretName": "ssh", "defaultMode": 256}},
                    ],
                },
            })
        );
    }

    #[test]
    fn the_build_container_has_exactly_the_agents_capabilities() {
        let pod = pod(&config());
        let build = container(&pod, BUILD);
        let security = build.security_context.as_ref().unwrap();
        assert_eq!(security.allow_privilege_escalation, Some(false));
        let caps = security.capabilities.as_ref().unwrap();
        assert_eq!(caps.drop.as_deref(), Some(&["ALL".to_string()][..]));
        let added: Vec<&str> = caps.add.iter().flatten().map(String::as_str).collect();
        assert_eq!(added, layout::CAPABILITIES);
    }

    #[test]
    fn stage_and_hold_are_unprivileged() {
        let pod = pod(&config());
        for which in [STAGE, HOLD] {
            let security = container(&pod, which).security_context.as_ref().unwrap();
            assert_eq!(security.run_as_non_root, Some(true), "{which}");
            assert_eq!(security.allow_privilege_escalation, Some(false), "{which}");
            assert_eq!(security.read_only_root_filesystem, Some(true), "{which}");
            let caps = security.capabilities.as_ref().unwrap();
            assert_eq!(
                caps.drop.as_deref(),
                Some(&["ALL".to_string()][..]),
                "{which}"
            );
            assert_eq!(caps.add, None, "{which}");
        }
    }

    #[test]
    fn the_pod_has_no_identity_and_the_default_seccomp_profile() {
        let pod = pod(&config());
        let spec = spec(&pod);
        assert_eq!(spec.service_account_name.as_deref(), Some("aurcache-build"));
        assert_eq!(spec.automount_service_account_token, Some(false));
        assert_eq!(spec.restart_policy.as_deref(), Some("Never"));
        let seccomp = spec
            .security_context
            .as_ref()
            .unwrap()
            .seccomp_profile
            .as_ref();
        assert_eq!(seccomp.unwrap().type_, "RuntimeDefault");
    }

    #[test]
    fn host_users_is_false_only_when_asked() {
        let mut cfg = config();
        assert_eq!(spec(&pod(&cfg)).host_users, None);
        cfg.user_namespaces = true;
        assert_eq!(spec(&pod(&cfg)).host_users, Some(false));
    }

    #[test]
    fn the_node_is_chosen_by_architecture() {
        for (arch, oci) in [("x86_64", "amd64"), ("aarch64", "arm64"), ("armv7h", "arm")] {
            let pod = render_with(&config(), 1, arch, None).unwrap();
            let selector = spec(&pod).node_selector.as_ref().unwrap();
            assert_eq!(selector["kubernetes.io/arch"], oci);
            assert_eq!(selector["kubernetes.io/os"], "linux");
            assert_eq!(
                pod.metadata.labels.as_ref().unwrap()["aurcache.io/arch"],
                arch
            );
        }
    }

    #[test]
    fn an_architecture_without_a_name_is_an_error() {
        let error = render_with(&config(), 1, "sparc", None).unwrap_err();
        assert!(error.to_string().contains("sparc"), "{error}");
    }

    #[test]
    fn the_deadline_follows_the_timeout() {
        let none = render_with(&config(), 1, "x86_64", None).unwrap();
        assert_eq!(spec(&none).active_deadline_seconds, None);
        let pod = render_with(&config(), 1, "x86_64", Some(60)).unwrap();
        assert_eq!(spec(&pod).active_deadline_seconds, Some(3660));
    }

    #[test]
    fn nothing_is_limited_unless_a_limit_is_set() {
        let pod = pod(&config());
        assert_eq!(container(&pod, BUILD).resources, None);
        let work = &spec(&pod).volumes.as_ref().unwrap()[0];
        assert_eq!(work.empty_dir.as_ref().unwrap().size_limit, None);
    }

    #[test]
    fn each_limit_appears_alone_and_is_also_the_request() {
        let only = |tunables: Tunables, line: &str| {
            let mut cfg = config();
            cfg.tunables = tunables;
            let pod = pod(&cfg);
            let resources = container(&pod, BUILD).resources.clone().unwrap();
            let requests = resources.requests.unwrap();
            let limits = resources.limits.unwrap();
            assert_eq!(requests, limits);
            assert_eq!(limits.keys().collect::<Vec<_>>(), [line]);
            limits[line].0.clone()
        };
        let base = config().tunables;
        let memory = Tunables {
            memory: Some(1 << 30),
            ..base.clone()
        };
        let cpus = Tunables {
            cpus: Some(0.25),
            ..base.clone()
        };
        let disk = Tunables {
            disk: Some(5 << 30),
            ..base
        };
        assert_eq!(only(memory, "memory"), "1073741824");
        assert_eq!(only(cpus, "cpu"), "250m");
        assert_eq!(only(disk, "ephemeral-storage"), "5368709120");
    }

    #[test]
    fn the_disk_limit_bounds_the_work_volume_too() {
        let pod = pod(&limited());
        let work = &spec(&pod).volumes.as_ref().unwrap()[0];
        assert_eq!(work.name, "work");
        let size = work.empty_dir.as_ref().unwrap().size_limit.as_ref();
        assert_eq!(size.unwrap().0, (30u64 << 30).to_string());
    }

    #[test]
    fn the_credential_is_mounted_into_build_alone() {
        let without = pod(&config());
        assert!(
            spec(&without)
                .volumes
                .iter()
                .flatten()
                .all(|v| v.secret.is_none())
        );

        let mut cfg = config();
        cfg.credential_secret = Some("ssh".into());
        let with = pod(&cfg);
        let secrets = spec(&with)
            .volumes
            .iter()
            .flatten()
            .filter(|v| v.secret.is_some())
            .count();
        assert_eq!(secrets, 1);
        for which in [STAGE, BUILD, HOLD] {
            let mounted = container(&with, which)
                .volume_mounts
                .iter()
                .flatten()
                .any(|m| m.name == "credentials");
            assert_eq!(mounted, which == BUILD, "{which}");
        }
    }

    /// What the executor is told about the credential is where the pod has it.
    #[test]
    fn the_credential_is_named_where_build_mounts_it() {
        assert_eq!(credentials(&config()), None);
        let mut cfg = config();
        cfg.credential_secret = Some("ssh".into());
        let named = credentials(&cfg).unwrap();
        let with = pod(&cfg);
        let mounted = container(&with, BUILD)
            .volume_mounts
            .iter()
            .flatten()
            .find(|m| m.name == "credentials")
            .unwrap();
        assert_eq!(Path::new(&mounted.mount_path), named);
        assert_eq!(mounted.read_only, Some(true));
    }

    /// What an exec names is what `stage` and `hold` themselves run.
    #[test]
    fn the_agent_is_named_where_stage_and_hold_run_it() {
        let pod = pod(&config());
        for which in [STAGE, HOLD] {
            let command = container(&pod, which).command.as_ref().unwrap();
            assert_eq!(command[0], agent_path(), "{which}");
        }
        assert!(agent_path().ends_with(layout::BIN_NAME));
    }

    #[test]
    fn the_operators_template_sits_under_ours() {
        let mut cfg = config();
        cfg.extras = serde_json::from_value(json!({
            "nodeSelector": {"pool": "builds", "kubernetes.io/arch": "s390x"},
            "labels": {"team": "a", "aurcache.io/dispatcher": "other"},
            "annotations": {"note": "x"},
            "priorityClassName": "low",
            "runtimeClassName": "gvisor",
            "schedulerName": "custom",
            "tolerations": [{"key": "builds", "operator": "Exists"}],
            "imagePullSecrets": [{"name": "pull"}],
        }))
        .unwrap();
        let pod = pod(&cfg);
        let spec = spec(&pod);
        let selector = spec.node_selector.as_ref().unwrap();
        assert_eq!(selector["pool"], "builds");
        assert_eq!(selector["kubernetes.io/arch"], "amd64");
        let labels = pod.metadata.labels.as_ref().unwrap();
        assert_eq!(labels["team"], "a");
        assert_eq!(labels["aurcache.io/dispatcher"], "abcdef0123456789");
        assert_eq!(pod.metadata.annotations.as_ref().unwrap()["note"], "x");
        assert_eq!(spec.priority_class_name.as_deref(), Some("low"));
        assert_eq!(spec.runtime_class_name.as_deref(), Some("gvisor"));
        assert_eq!(spec.scheduler_name.as_deref(), Some("custom"));
        assert_eq!(spec.tolerations.as_ref().unwrap().len(), 1);
        assert_eq!(spec.image_pull_secrets.as_ref().unwrap()[0].name, "pull");
    }

    #[test]
    fn a_template_cannot_ask_for_what_baseline_forbids() {
        for field in [
            "privileged",
            "hostNetwork",
            "hostPID",
            "hostIPC",
            "volumes",
            "securityContext",
            "containers",
            "hostUsers",
            "fsGroup",
        ] {
            let parsed = serde_json::from_value::<PodExtras>(json!({ field: true }));
            assert!(parsed.is_err(), "{field}");
        }
    }

    fn keys(value: &Value, found: &mut Vec<String>) {
        match value {
            Value::Object(map) => {
                for (key, inner) in map {
                    found.push(key.clone());
                    keys(inner, found);
                }
            }
            Value::Array(items) => items.iter().for_each(|item| keys(item, found)),
            _ => {}
        }
    }

    /// Whatever the settings and the template, a pod stays within `baseline`.
    #[test]
    fn no_combination_renders_what_baseline_forbids() {
        let template = serde_json::from_value::<PodExtras>(json!({
            "nodeSelector": {"a": "b"}, "labels": {"c": "d"},
            "tolerations": [{"operator": "Exists"}],
        }))
        .unwrap();
        for bits in 0..32u32 {
            let mut cfg = config();
            cfg.user_namespaces = bits & 1 != 0;
            cfg.credential_secret = (bits & 2 != 0).then(|| "ssh".to_string());
            cfg.tunables.memory = (bits & 4 != 0).then_some(1 << 30);
            cfg.tunables.disk = (bits & 8 != 0).then_some(1 << 30);
            if bits & 16 != 0 {
                cfg.extras = template.clone();
            }
            let value = serde_json::to_value(pod(&cfg)).unwrap();
            let mut found = Vec::new();
            keys(&value, &mut found);
            for forbidden in [
                "privileged",
                "hostPath",
                "hostNetwork",
                "hostPID",
                "hostIPC",
                "fsGroup",
            ] {
                assert!(
                    !found.iter().any(|k| k == forbidden),
                    "{forbidden} in {bits}"
                );
            }
        }
    }

    #[test]
    fn names_and_label_values_are_valid_for_the_largest_build_id() {
        let pod = render_with(&config(), i32::MAX, "x86_64", None).unwrap();
        let name = pod.metadata.name.unwrap();
        let dns_label = |text: &str| {
            !text.is_empty()
                && text.len() <= 63
                && text
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
                && !text.starts_with('-')
                && !text.ends_with('-')
        };
        assert!(dns_label(&name), "{name}");
        for (key, value) in pod.metadata.labels.unwrap() {
            let alphabet = value
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
            assert!(value.len() <= 63 && alphabet, "{key}={value}");
        }
    }

    #[test]
    fn the_agent_comes_from_the_given_image_in_stage_and_hold() {
        let pod = pod(&config());
        for which in [STAGE, HOLD] {
            assert_eq!(
                container(&pod, which).image.as_deref(),
                Some("reg/agent@sha256:1")
            );
        }
        assert_eq!(
            container(&pod, BUILD).image.as_deref(),
            Some("reg/builder:1")
        );
    }
}
