//! A build pod, rendered from the configuration and a job, with nothing in
//! between that needs a cluster.

use crate::config::{Config, PodExtras};
use anyhow::{Result, anyhow};
use aurcache_build_agent::layout;
use aurcache_common::worker::JobDescriptor;
use aurcache_worker_container::arch;
use aurcache_worker_core::protocol::Timeout;
use k8s_openapi::api::core::v1::{
    Capabilities, Container, EmptyDirVolumeSource, Pod, PodSecurityContext, PodSpec,
    ResourceRequirements, SeccompProfile, SecurityContext, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use std::collections::BTreeMap;
use std::time::Duration;

/// `app.kubernetes.io/managed-by` on every pod that this worker creates.
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
/// it.
const DEADLINE_GRACE: Duration = Duration::from_secs(60 * 60);
/// The timeout assumed for a build that has none.
const UNTIMED_LIMIT: Duration = Duration::from_secs(24 * 60 * 60);

const LABEL_MANAGED_BY: &str = "app.kubernetes.io/managed-by";
const LABEL_DISPATCHER: &str = "aurcache.io/dispatcher";
const LABEL_BUILD: &str = "aurcache.io/build-id";
const LABEL_ARCH: &str = "aurcache.io/arch";
const NODE_OS: &str = "kubernetes.io/os";
const NODE_ARCH: &str = "kubernetes.io/arch";

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

/// `app.kubernetes.io/managed-by=aurcache-worker-kube`: every pod that a
/// dispatcher made, whichever dispatcher that was.
#[must_use]
pub fn managed_selector() -> String {
    format!("{LABEL_MANAGED_BY}={MANAGED_BY}")
}

/// The labels that every pod of this dispatcher carries.
#[must_use]
pub fn labels(dispatcher: &str) -> BTreeMap<String, String> {
    BTreeMap::from([
        (LABEL_MANAGED_BY.to_string(), MANAGED_BY.to_string()),
        (LABEL_DISPATCHER.to_string(), dispatcher.to_string()),
    ])
}

fn quantity(text: impl Into<String>) -> Quantity {
    Quantity(text.into())
}

/// CPUs as a quantity, in milli-CPUs rounded up.
fn cpu_quantity(cpus: f64) -> Quantity {
    // Saturating: the cast cannot fail.
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
    if let Some(bytes) = cfg.tunables.memory_bytes {
        lines.insert("memory".to_string(), quantity(bytes.to_string()));
    }
    if let Some(bytes) = cfg.tunables.disk_bytes {
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

/// What `build` runs as: root, with the seven capabilities that the root step
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

fn mount(name: &str, path: &str) -> VolumeMount {
    VolumeMount {
        name: name.to_string(),
        mount_path: path.to_string(),
        ..VolumeMount::default()
    }
}

fn read_only(name: &str, path: &str) -> VolumeMount {
    VolumeMount {
        read_only: Some(true),
        ..mount(name, path)
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
fn helper_resources() -> ResourceRequirements {
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

fn stage(cfg: &Config) -> Container {
    Container {
        name: STAGE.to_string(),
        image: Some(cfg.agent_image.clone()),
        image_pull_policy: Some(cfg.pull_policy.clone()),
        command: Some(
            [&agent_path(), "stage", "--work", WORK, "--tools", TOOLS]
                .map(String::from)
                .to_vec(),
        ),
        security_context: Some(unprivileged()),
        resources: Some(helper_resources()),
        volume_mounts: Some(vec![mount("work", WORK), mount("tools", TOOLS)]),
        ..Container::default()
    }
}

fn build(cfg: &Config) -> Container {
    Container {
        name: BUILD.to_string(),
        image: Some(cfg.builder_image.clone()),
        image_pull_policy: Some(cfg.pull_policy.clone()),
        command: Some(vec![
            build_agent_path(),
            "build".to_string(),
            "--work".to_string(),
            WORK.to_string(),
            "--log-file".to_string(),
        ]),
        security_context: Some(build_security()),
        resources: resources(cfg),
        volume_mounts: Some(vec![mount("work", WORK), read_only("tools", TOOLS)]),
        ..Container::default()
    }
}

fn hold(cfg: &Config) -> Container {
    Container {
        name: HOLD.to_string(),
        image: Some(cfg.agent_image.clone()),
        image_pull_policy: Some(cfg.pull_policy.clone()),
        // As PID 1 it ignores SIGTERM, which is why pods are deleted with a short grace.
        command: Some(["sleep", "infinity"].map(String::from).to_vec()),
        security_context: Some(unprivileged()),
        resources: Some(helper_resources()),
        volume_mounts: Some(vec![read_only("work", WORK)]),
        ..Container::default()
    }
}

fn volumes(cfg: &Config) -> Vec<Volume> {
    vec![
        Volume {
            name: "work".to_string(),
            empty_dir: Some(EmptyDirVolumeSource {
                size_limit: cfg
                    .tunables
                    .disk_bytes
                    .map(|bytes| quantity(bytes.to_string())),
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

fn pod_labels(cfg: &Config, job: &JobDescriptor) -> BTreeMap<String, String> {
    let mut all = labels(&cfg.dispatcher);
    all.insert(LABEL_BUILD.to_string(), job.build_id.to_string());
    all.insert(LABEL_ARCH.to_string(), job.arch.clone());
    all
}

/// The node selector: the operator's under the OS and architecture.
fn node_selector(extras: &PodExtras, oci_arch: &str) -> BTreeMap<String, String> {
    let mut all = extras.node_selector.clone().unwrap_or_default();
    all.insert(NODE_OS.to_string(), "linux".to_string());
    all.insert(NODE_ARCH.to_string(), oci_arch.to_string());
    all
}

/// The build's timeout plus the grace, at most what the API server accepts.
/// The worker stops every build that has a timeout sooner, counting from the
/// pod's creation, so this ends a pod that the worker left behind, and gives
/// a hand-back at least an hour past the build's timeout.
fn deadline(timeout: Option<Timeout>) -> i64 {
    let total = timeout
        .map_or(UNTIMED_LIMIT, |timeout| timeout.limit)
        .saturating_add(DEADLINE_GRACE);
    i64::from(i32::try_from(total.as_secs()).unwrap_or(i32::MAX))
}

/// The pod for one build that may run for `timeout`. An error for an
/// architecture with no OCI name.
pub fn render(cfg: &Config, job: &JobDescriptor, timeout: Option<Timeout>) -> Result<Pod> {
    let job_arch = &job.arch;
    let platform = arch::platform(job_arch)
        .ok_or_else(|| anyhow!("architecture {job_arch:?} has no Kubernetes name"))?;
    let extras = &cfg.extras;
    Ok(Pod {
        metadata: ObjectMeta {
            name: Some(name(&cfg.dispatcher, job.build_id)),
            namespace: Some(cfg.namespace.clone()),
            labels: Some(pod_labels(cfg, job)),
            ..ObjectMeta::default()
        },
        spec: Some(PodSpec {
            restart_policy: Some("Never".to_string()),
            active_deadline_seconds: Some(deadline(timeout)),
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
            init_containers: Some(vec![stage(cfg)]),
            containers: vec![build(cfg), hold(cfg)],
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
    use serde_json::json;

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
                memory_bytes: None,
                cpus: None,
                disk_bytes: None,
                schedule_timeout: Duration::from_secs(600),
                keyserver: "hkps://k".into(),
            },
        }
    }

    fn job(build_id: i32, arch: &str, timeout_secs: Option<u64>) -> JobDescriptor {
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
            timeout_secs,
            expected_version: "1-1".into(),
            ssh_key: None,
        }
    }

    fn render_with(cfg: &Config, build_id: i32, arch: &str, timeout: Option<u64>) -> Result<Pod> {
        let job = job(build_id, arch, timeout);
        render(cfg, &job, Timeout::for_job(None, &job))
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
    /// SSH key here, and none of it reaches the pod. Every container states
    /// its CPU and memory: a ResourceQuota on either refuses a pod with a
    /// container that leaves it out.
    #[test]
    fn the_whole_pod() {
        let mut cfg = config();
        cfg.tunables.memory_bytes = Some(8 << 30);
        cfg.tunables.cpus = Some(1.5);
        cfg.tunables.disk_bytes = Some(30 << 30);
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
                    "activeDeadlineSeconds": 7200,
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
                                        "--work", "/work", "--log-file"],
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
        assert_eq!(spec(&pod).active_deadline_seconds, Some(3660));
        for none in [None, Some(0)] {
            let pod = render_with(&config(), 1, "x86_64", none).unwrap();
            assert_eq!(spec(&pod).active_deadline_seconds, Some(25 * 3600));
        }
        let pod = render_with(&config(), 1, "x86_64", Some(u64::MAX)).unwrap();
        assert_eq!(spec(&pod).active_deadline_seconds, Some(i32::MAX.into()));
    }

    #[test]
    fn nothing_is_limited_unless_a_limit_is_set() {
        let pod = pod(&config());
        assert_eq!(build_container(&pod).resources, None);
        let work = &spec(&pod).volumes.as_ref().unwrap()[0];
        assert_eq!(work.empty_dir.as_ref().unwrap().size_limit, None);
    }

    /// A limit set alone is the only line, as its request too, and a CPU
    /// fraction rounds up to whole millicores.
    #[test]
    fn a_limit_alone_is_the_only_line() {
        let mut cfg = config();
        cfg.tunables.cpus = Some(0.2501);
        let pod = pod(&cfg);
        let resources = build_container(&pod).resources.clone().unwrap();
        let only = BTreeMap::from([("cpu".to_string(), Quantity("251m".to_string()))]);
        assert_eq!(resources.limits, Some(only.clone()));
        assert_eq!(resources.requests, Some(only));
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

    /// What Pod Security's `baseline` level lets a container add.
    const BASELINE_CAPABILITIES: [&str; 13] = [
        "AUDIT_WRITE",
        "CHOWN",
        "DAC_OVERRIDE",
        "FOWNER",
        "FSETID",
        "KILL",
        "MKNOD",
        "NET_BIND_SERVICE",
        "SETFCAP",
        "SETGID",
        "SETPCAP",
        "SETUID",
        "SYS_CHROOT",
    ];

    /// The pod-level fields that `baseline` forbids are in the snapshot; this
    /// is what each container could add.
    #[test]
    fn every_container_stays_within_baseline() {
        let pod = pod(&config());
        let spec = spec(&pod);
        for container in spec
            .init_containers
            .iter()
            .flatten()
            .chain(&spec.containers)
        {
            let security = container.security_context.as_ref().unwrap();
            let added = security.capabilities.as_ref().and_then(|c| c.add.as_ref());
            assert!(
                added
                    .into_iter()
                    .flatten()
                    .all(|cap| BASELINE_CAPABILITIES.contains(&cap.as_str())),
                "{}",
                container.name
            );
            assert_eq!(security.privileged, None, "{}", container.name);
            assert_eq!(security.proc_mount, None, "{}", container.name);
            let mut ports = container.ports.iter().flatten();
            assert!(
                ports.all(|port| port.host_port.is_none()),
                "{}",
                container.name
            );
        }
    }

    #[test]
    fn the_name_of_the_largest_build_id_fits_a_dns_label() {
        let pod = render_with(&config(), i32::MAX, "x86_64", None).unwrap();
        assert!(pod.metadata.name.unwrap().len() <= 63);
    }
}
