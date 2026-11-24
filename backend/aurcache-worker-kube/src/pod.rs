//! A build pod, rendered from the configuration and a job, with nothing in
//! between that needs a cluster.

use crate::config::{Config, PodExtras};
use anyhow::{Result, anyhow};
use aurcache_build_agent::layout::{self, Work};
use aurcache_common::worker::JobDescriptor;
use aurcache_worker_container::arch;
use k8s_openapi::api::core::v1::{
    Capabilities, Container, EmptyDirVolumeSource, Pod, PodSecurityContext, PodSpec,
    ResourceRequirements, SeccompProfile, SecurityContext, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use std::collections::BTreeMap;
use std::time::Duration;

/// `app.kubernetes.io/managed-by` on every pod this worker creates.
pub const MANAGED_BY: &str = "aurcache-worker-kube";
/// The container that runs the build.
pub const BUILD: &str = "build";
/// The init container that installs the agent and receives the job.
pub const STAGE: &str = "stage";
/// The container that keeps the pod up for the hand-back, and that the log
/// and the packages are read through.
pub const HOLD: &str = "hold";
/// Where the work volume is mounted, in every container.
pub const WORK: &str = "/work";
/// The ServiceAccount that build pods run as: bound to nothing, token never mounted.
pub const SERVICE_ACCOUNT: &str = "aurcache-build";

const TOOLS: &str = "/aurcache/bin";
/// Where the agent lives in the dispatcher's image.
const AGENT_IN_IMAGE: &str = "/usr/local/bin";
/// The user that `stage` and `hold` run as.
const NOBODY: i64 = 65534;
/// How long a build pod outlives its build's timeout before the kubelet ends
/// it: the server's `REAP_BACKSTOP_GRACE`, after which it takes nothing more
/// of the build.
const DEADLINE_GRACE: Duration = Duration::from_secs(5 * 60);
/// The deadline of a pod whose build has no timeout, so that a pod the
/// dispatcher never deleted does not hold its place in a quota for ever.
const UNTIMED_DEADLINE: Duration = Duration::from_secs(24 * 60 * 60);

const LABEL_MANAGED_BY: &str = "app.kubernetes.io/managed-by";
const LABEL_DISPATCHER: &str = "aurcache.io/dispatcher";
const LABEL_BUILD: &str = "aurcache.io/build-id";
const LABEL_ARCH: &str = "aurcache.io/arch";
const NODE_OS: &str = "kubernetes.io/os";
const NODE_ARCH: &str = "kubernetes.io/arch";

/// What a build pod is rendered from.
pub struct PodInputs<'a> {
    pub cfg: &'a Config,
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

/// `app.kubernetes.io/managed-by=aurcache-worker-kube`: every pod a
/// dispatcher made, whichever dispatcher that was.
#[must_use]
pub fn managed_selector() -> String {
    format!("{LABEL_MANAGED_BY}={MANAGED_BY}")
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
/// `stage` runs, and what an exec into `stage` or `hold` names.
#[must_use]
pub fn agent_path() -> String {
    format!("{AGENT_IN_IMAGE}/{}", layout::BIN_NAME)
}

/// The agent, where the build container has it: what an exec into `build`
/// names.
#[must_use]
pub fn build_agent_path() -> String {
    format!("{TOOLS}/{}", layout::BIN_NAME)
}

/// What a container that only runs the agent's own subcommands asks for.
///
/// Stated in full, since a ResourceQuota on CPU or memory refuses a pod with
/// a container that leaves a line out. One CPU is what sending the packages
/// uses.
#[must_use]
pub fn helper_resources() -> ResourceRequirements {
    ResourceRequirements {
        requests: Some(BTreeMap::from([
            ("cpu".to_string(), quantity("10m")),
            ("memory".to_string(), quantity("16Mi")),
        ])),
        limits: Some(BTreeMap::from([
            ("cpu".to_string(), quantity("1")),
            ("memory".to_string(), quantity("64Mi")),
        ])),
        ..ResourceRequirements::default()
    }
}

fn stage(inputs: &PodInputs<'_>) -> Container {
    Container {
        name: STAGE.to_string(),
        image: Some(inputs.cfg.agent_image.clone()),
        image_pull_policy: Some(inputs.cfg.pull_policy.clone()),
        command: Some(
            [&agent_path(), "stage", "--work", WORK, "--tools", TOOLS]
                .map(String::from)
                .to_vec(),
        ),
        security_context: Some(unprivileged()),
        resources: Some(helper_resources()),
        volume_mounts: Some(vec![
            mount("work", WORK, false),
            mount("tools", TOOLS, false),
        ]),
        ..Container::default()
    }
}

fn build(inputs: &PodInputs<'_>) -> Container {
    let log = Work::new(WORK).log();
    let mounts = vec![mount("work", WORK, false), mount("tools", TOOLS, true)];
    Container {
        name: BUILD.to_string(),
        image: Some(inputs.cfg.builder_image.clone()),
        image_pull_policy: Some(inputs.cfg.pull_policy.clone()),
        command: Some(vec![
            build_agent_path(),
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
    Container {
        name: HOLD.to_string(),
        image: Some(inputs.cfg.agent_image.clone()),
        image_pull_policy: Some(inputs.cfg.pull_policy.clone()),
        // As PID 1 it ignores SIGTERM, so a deleted pod waits out its grace.
        command: Some(["sleep", "infinity"].map(String::from).to_vec()),
        security_context: Some(unprivileged()),
        resources: Some(helper_resources()),
        volume_mounts: Some(vec![mount("work", WORK, true)]),
        ..Container::default()
    }
}

fn volumes(cfg: &Config) -> Vec<Volume> {
    vec![
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
    ]
}

fn pod_labels(inputs: &PodInputs<'_>, arch: &str) -> BTreeMap<String, String> {
    let mut all = labels(&inputs.cfg.dispatcher);
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

fn deadline(timeout: Option<Duration>) -> i64 {
    let total = timeout
        .unwrap_or(UNTIMED_DEADLINE)
        .saturating_add(DEADLINE_GRACE);
    i64::try_from(total.as_secs()).unwrap_or(i64::MAX)
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
            ..ObjectMeta::default()
        },
        spec: Some(PodSpec {
            restart_policy: Some("Never".to_string()),
            active_deadline_seconds: Some(deadline(inputs.timeout)),
            service_account_name: Some(SERVICE_ACCOUNT.to_string()),
            automount_service_account_token: Some(false),
            enable_service_links: Some(false),
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
            image_pull_secrets: extras.image_pull_secrets.clone(),
            ..PodSpec::default()
        }),
        ..Pod::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Tunables;
    use aurcache_common::worker::MirrorlistOffer;
    use serde_json::{Value, json};

    fn config() -> Config {
        Config {
            namespace: "aurcache-builds".into(),
            dispatcher: "abcdef0123456789".into(),
            builder_image: "reg/builder:1".into(),
            pull_policy: "Always".into(),
            agent_image: "reg/agent@sha256:1".into(),
            credential: None,
            extras: PodExtras::default(),
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
            ssh_key: None,
        }
    }

    fn render_with(cfg: &Config, build_id: i32, arch: &str, timeout: Option<u64>) -> Result<Pod> {
        render(&PodInputs {
            cfg,
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

    fn build_container(pod: &Pod) -> &Container {
        spec(pod)
            .containers
            .iter()
            .find(|c| c.name == BUILD)
            .unwrap()
    }

    fn credential() -> aurcache_worker_container::ssh::CredentialFiles {
        aurcache_worker_container::ssh::CredentialFiles {
            key: "/etc/aurcache/ssh/ssh-privatekey".into(),
            known_hosts: "/etc/aurcache/ssh/known_hosts".into(),
        }
    }

    /// The whole pod, as the API server receives it. The dispatcher holds an
    /// SSH key here, and none of it reaches the pod.
    #[test]
    fn the_whole_pod() {
        let mut cfg = config();
        cfg.tunables.memory = Some(8 << 30);
        cfg.tunables.cpus = Some(1.5);
        cfg.tunables.disk = Some(30 << 30);
        cfg.credential = Some(credential());
        let value =
            serde_json::to_value(render_with(&cfg, 7, "aarch64", Some(3600)).unwrap()).unwrap();
        let unprivileged = json!({
            "runAsUser": 65534, "runAsGroup": 65534, "runAsNonRoot": true,
            "allowPrivilegeEscalation": false, "readOnlyRootFilesystem": true,
            "capabilities": {"drop": ["ALL"]},
        });
        let helper = json!({
            "requests": {"cpu": "10m", "memory": "16Mi"},
            "limits": {"cpu": "1", "memory": "64Mi"},
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
                    "activeDeadlineSeconds": 3900,
                    "serviceAccountName": "aurcache-build",
                    "automountServiceAccountToken": false,
                    "enableServiceLinks": false,
                    "nodeSelector": {"kubernetes.io/os": "linux", "kubernetes.io/arch": "arm64"},
                    "securityContext": {"seccompProfile": {"type": "RuntimeDefault"}},
                    "initContainers": [{
                        "name": "stage",
                        "image": "reg/agent@sha256:1",
                        "imagePullPolicy": "Always",
                        "command": ["/usr/local/bin/aurcache-build-agent", "stage",
                                    "--work", "/work", "--tools", "/aurcache/bin"],
                        "securityContext": unprivileged,
                        "resources": helper,
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
                            ],
                        },
                        {
                            "name": "hold",
                            "image": "reg/agent@sha256:1",
                            "imagePullPolicy": "Always",
                            "command": ["sleep", "infinity"],
                            "securityContext": unprivileged,
                            "resources": helper,
                            "volumeMounts": [{"name": "work", "mountPath": "/work",
                                              "readOnly": true}],
                        },
                    ],
                    "volumes": [
                        {"name": "work", "emptyDir": {"sizeLimit": "32212254720"}},
                        {"name": "tools", "emptyDir": {"sizeLimit": "64Mi"}},
                    ],
                },
            })
        );
    }

    #[test]
    fn an_architecture_without_a_name_is_an_error() {
        let error = render_with(&config(), 1, "sparc", None).unwrap_err();
        assert!(error.to_string().contains("sparc"), "{error}");
    }

    /// Every pod has a deadline, a generous one when its build has none.
    #[test]
    fn the_deadline_follows_the_timeout() {
        let pod = render_with(&config(), 1, "x86_64", Some(60)).unwrap();
        assert_eq!(spec(&pod).active_deadline_seconds, Some(360));
        let none = render_with(&config(), 1, "x86_64", None).unwrap();
        assert_eq!(spec(&none).active_deadline_seconds, Some(24 * 3600 + 300));
    }

    /// A ResourceQuota on a compute resource refuses a pod with a container
    /// that does not state it. With the worker's limits set, every container
    /// states all four lines.
    #[test]
    fn every_container_states_its_cpu_and_memory_once_the_build_is_limited() {
        let mut cfg = config();
        cfg.tunables.cpus = Some(2.0);
        cfg.tunables.memory = Some(4 << 30);
        let pod = pod(&cfg);
        let spec = spec(&pod);
        let all = spec
            .init_containers
            .iter()
            .flatten()
            .chain(&spec.containers);
        let mut seen = 0;
        for container in all {
            let resources = container.resources.as_ref().unwrap();
            for lines in [&resources.requests, &resources.limits] {
                let lines = lines.as_ref().unwrap();
                for line in ["cpu", "memory"] {
                    assert!(lines.contains_key(line), "{}: {line}", container.name);
                }
            }
            seen += 1;
        }
        assert_eq!(seen, 3);
    }

    #[test]
    fn nothing_is_limited_unless_a_limit_is_set() {
        let pod = pod(&config());
        assert_eq!(build_container(&pod).resources, None);
        let work = &spec(&pod).volumes.as_ref().unwrap()[0];
        assert_eq!(work.empty_dir.as_ref().unwrap().size_limit, None);
    }

    #[test]
    fn each_limit_appears_alone_and_is_also_the_request() {
        let only = |tunables: Tunables, line: &str| {
            let mut cfg = config();
            cfg.tunables = tunables;
            let pod = pod(&cfg);
            let resources = build_container(&pod).resources.clone().unwrap();
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
    fn the_operators_template_sits_under_ours() {
        let mut cfg = config();
        cfg.extras = serde_json::from_value(json!({
            "nodeSelector": {"pool": "builds", "kubernetes.io/arch": "s390x"},
            "tolerations": [{"key": "builds", "operator": "Exists"}],
            "imagePullSecrets": [{"name": "pull"}],
        }))
        .unwrap();
        let pod = pod(&cfg);
        let spec = spec(&pod);
        let selector = spec.node_selector.as_ref().unwrap();
        assert_eq!(selector["pool"], "builds");
        assert_eq!(selector["kubernetes.io/arch"], "amd64");
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
            "nodeSelector": {"a": "b"},
            "tolerations": [{"operator": "Exists"}],
        }))
        .unwrap();
        for bits in 0..16u32 {
            let mut cfg = config();
            cfg.credential = (bits & 1 != 0).then(credential);
            cfg.tunables.memory = (bits & 2 != 0).then_some(1 << 30);
            cfg.tunables.disk = (bits & 4 != 0).then_some(1 << 30);
            if bits & 8 != 0 {
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
}
