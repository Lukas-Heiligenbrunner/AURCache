//! The dispatcher's configuration: its environment, and what each build
//! takes from the declared settings.
//!
//! The variables are in `kubernetes-executor-plan.md`, section 4.6.

use crate::settings::{DEFAULT_SCHEDULE_TIMEOUT, keys};
use anyhow::{Context, Result, bail};
use aurcache_worker_container::ssh::CredentialFiles;
use aurcache_worker_core::config::{CoreConfig, env_opt};
use aurcache_worker_core::settings::{DEFAULT_KEYSERVER, WorkerSettings};
use k8s_openapi::api::core::v1::{
    Affinity, LocalObjectReference, Toleration, TopologySpreadConstraint,
};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::time::Duration;

/// Where build pods go when nothing says otherwise.
const DEFAULT_NAMESPACE: &str = "aurcache-builds";
/// The build container's image when nothing says otherwise.
const DEFAULT_BUILDER_IMAGE: &str = "ghcr.io/lukas-heiligenbrunner/aurcache-builder-v2:latest";
/// The pull policies Kubernetes knows.
const PULL_POLICIES: [&str; 3] = ["Always", "IfNotPresent", "Never"];

/// What every worker reads, advertising the dispatcher's own architecture
/// unless `WORKER_ARCHES` says otherwise.
#[must_use]
pub fn core_from_env() -> CoreConfig {
    CoreConfig::from_env(
        aurcache_common::version::full_version(env!("CARGO_PKG_VERSION")),
        crate::settings::kube_settings(),
    )
}

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub namespace: String,
    /// The label value every pod of this dispatcher carries: the first 16
    /// hex digits of its fingerprint.
    pub dispatcher: String,
    pub builder_image: String,
    /// The pull policy of every image a pod of this worker's runs: the
    /// builder's, and the agent's.
    pub pull_policy: String,
    /// The image of the `stage` and `hold` containers, which the build agent
    /// comes from (`WORKER_KUBE_AGENT_IMAGE`).
    pub agent_image: String,
    /// The SSH key builds fetch their sources with, which the dispatcher
    /// holds and answers for (`WORKER_GIT_SSH_KEY`, `WORKER_SSH_KNOWN_HOSTS`).
    pub credential: Option<CredentialFiles>,
    pub user_namespaces: bool,
    pub extras: PodExtras,
    pub tunables: Tunables,
}

/// What each build takes from the declared settings.
#[derive(Clone, Debug, PartialEq)]
pub struct Tunables {
    pub memory: Option<u64>,
    pub cpus: Option<f64>,
    pub disk: Option<u64>,
    pub schedule_timeout: Duration,
    pub keyserver: String,
}

impl Tunables {
    /// Read the limits and the rest from `settings`. A limit of zero is no
    /// limit.
    #[must_use]
    pub fn from_settings(settings: &WorkerSettings) -> Self {
        Self {
            memory: settings.size(keys::BUILD_MEMORY_MAX).filter(|n| *n > 0),
            cpus: settings
                .float(keys::BUILD_CPUS)
                .filter(|n| n.is_finite() && *n > 0.0),
            disk: settings.size(keys::BUILD_DISK_MAX).filter(|n| *n > 0),
            schedule_timeout: settings
                .duration(keys::SCHEDULE_TIMEOUT)
                .unwrap_or(DEFAULT_SCHEDULE_TIMEOUT),
            keyserver: settings
                .raw(keys::KEYSERVER)
                .map_or_else(|| DEFAULT_KEYSERVER.to_string(), str::to_string),
        }
    }
}

/// The fields of a build pod an operator may add, from the JSON file
/// `WORKER_KUBE_POD_TEMPLATE` names: scheduling, labels and annotations,
/// never a volume, a container or a security context. An unknown field is an
/// error.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct PodExtras {
    pub tolerations: Option<Vec<Toleration>>,
    pub affinity: Option<Affinity>,
    pub node_selector: Option<BTreeMap<String, String>>,
    pub priority_class_name: Option<String>,
    pub runtime_class_name: Option<String>,
    pub image_pull_secrets: Option<Vec<LocalObjectReference>>,
    pub scheduler_name: Option<String>,
    pub topology_spread_constraints: Option<Vec<TopologySpreadConstraint>>,
    pub labels: Option<BTreeMap<String, String>>,
    pub annotations: Option<BTreeMap<String, String>>,
}

impl PodExtras {
    /// Read the template file, or nothing at all when there is none.
    fn load(path: Option<&str>) -> Result<Self> {
        let Some(path) = path else {
            return Ok(Self::default());
        };
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading WORKER_KUBE_POD_TEMPLATE {path}"))?;
        serde_json::from_str(&text)
            .with_context(|| format!("parsing WORKER_KUBE_POD_TEMPLATE {path}"))
    }
}

/// A yes/no variable: `true` or `false`, in any case.
fn flag(name: &str, raw: Option<String>, default: bool) -> Result<bool> {
    match raw.map(|v| v.to_ascii_lowercase()).as_deref() {
        None => Ok(default),
        Some("true") => Ok(true),
        Some("false") => Ok(false),
        Some(other) => bail!("{name} is {other:?}; expected true or false"),
    }
}

impl Config {
    /// Read the environment, with `settings` the worker's resolution of its
    /// declared settings and `fingerprint` the dispatcher's own.
    ///
    /// # Errors
    ///
    /// When a variable has no valid reading, the pod template cannot be read
    /// or has a field outside the allowed ones, or no agent image is named.
    pub fn from_env(settings: &WorkerSettings, fingerprint: &str) -> Result<Self> {
        Self::from_vars(settings, fingerprint, env_opt)
    }

    /// [`Self::from_env`] over any source of variables.
    fn from_vars(
        settings: &WorkerSettings,
        fingerprint: &str,
        var: impl Fn(&str) -> Option<String>,
    ) -> Result<Self> {
        let pull_policy =
            var("WORKER_KUBE_IMAGE_PULL_POLICY").unwrap_or_else(|| "Always".to_string());
        if !PULL_POLICIES.contains(&pull_policy.as_str()) {
            bail!(
                "WORKER_KUBE_IMAGE_PULL_POLICY is {pull_policy:?}; expected one of {}",
                PULL_POLICIES.join(", ")
            );
        }
        let agent_image = var("WORKER_KUBE_AGENT_IMAGE").context(
            "WORKER_KUBE_AGENT_IMAGE is not set: name the image build pods take the build agent \
             from, this worker's own",
        )?;
        Ok(Self {
            namespace: var("WORKER_KUBE_NAMESPACE").unwrap_or_else(|| DEFAULT_NAMESPACE.into()),
            dispatcher: aurcache_worker_container::tag::of(fingerprint)?,
            builder_image: var("WORKER_KUBE_BUILDER_IMAGE")
                .unwrap_or_else(|| DEFAULT_BUILDER_IMAGE.into()),
            pull_policy,
            agent_image,
            credential: CredentialFiles::from_vars(&var)?,
            user_namespaces: flag(
                "WORKER_KUBE_USER_NAMESPACES",
                var("WORKER_KUBE_USER_NAMESPACES"),
                false,
            )?,
            extras: PodExtras::load(var("WORKER_KUBE_POD_TEMPLATE").as_deref())?,
            tunables: Tunables::from_settings(settings),
        })
    }

    /// This configuration with the tunables of a new resolution of the
    /// settings, for the next build.
    #[must_use]
    pub fn with_settings(&self, settings: &WorkerSettings) -> Self {
        Self {
            tunables: Tunables::from_settings(settings),
            ..self.clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aurcache_common::worker_config::ConfigSnapshot;
    use std::collections::HashMap;

    const FINGERPRINT: &str = "AB:CD:EF:01:23:45:67:89:ab:cd:ef:01:23:45:67:89:00:11";

    fn declared() -> WorkerSettings {
        WorkerSettings::from_env(aurcache_worker_core::settings::protocol_settings())
            .extended(crate::settings::kube_settings())
    }

    fn snapshot(pairs: &[(&str, &str)]) -> ConfigSnapshot {
        ConfigSnapshot {
            revision: format!("{pairs:?}"),
            settings: pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
        }
    }

    fn read(pairs: &[(&str, &str)]) -> Result<Config> {
        let vars: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        Config::from_vars(&declared(), FINGERPRINT, |k| vars.get(k).cloned())
    }

    const AGENT: [(&str, &str); 1] = [("WORKER_KUBE_AGENT_IMAGE", "reg/agent:1")];

    #[test]
    fn defaults() {
        let cfg = read(&AGENT).unwrap();
        assert_eq!(cfg.namespace, "aurcache-builds");
        assert_eq!(cfg.dispatcher, "abcdef0123456789");
        assert_eq!(cfg.builder_image, DEFAULT_BUILDER_IMAGE);
        assert_eq!(cfg.pull_policy, "Always");
        assert_eq!(cfg.agent_image, "reg/agent:1");
        assert_eq!(cfg.credential, None);
        assert!(!cfg.user_namespaces);
        assert_eq!(cfg.extras, PodExtras::default());
        assert_eq!(
            cfg.tunables,
            Tunables {
                memory: None,
                cpus: None,
                disk: None,
                schedule_timeout: Duration::from_secs(600),
                keyserver: DEFAULT_KEYSERVER.into(),
            }
        );
    }

    #[test]
    fn no_agent_image_is_an_error_naming_its_variable() {
        let message = read(&[]).unwrap_err().to_string();
        assert!(message.contains("WORKER_KUBE_AGENT_IMAGE"), "{message}");
    }

    #[test]
    fn the_machines_choices() {
        let mut vars = AGENT.to_vec();
        vars.extend([
            ("WORKER_KUBE_NAMESPACE", "b"),
            ("WORKER_KUBE_IMAGE_PULL_POLICY", "IfNotPresent"),
            ("WORKER_GIT_SSH_KEY", "/etc/aurcache/ssh/ssh-privatekey"),
            ("WORKER_SSH_KNOWN_HOSTS", "/etc/aurcache/ssh/known_hosts"),
            ("WORKER_KUBE_USER_NAMESPACES", "TRUE"),
        ]);
        let cfg = read(&vars).unwrap();
        assert_eq!(cfg.namespace, "b");
        assert_eq!(cfg.pull_policy, "IfNotPresent");
        assert_eq!(
            cfg.credential,
            Some(CredentialFiles {
                key: "/etc/aurcache/ssh/ssh-privatekey".into(),
                known_hosts: "/etc/aurcache/ssh/known_hosts".into(),
            })
        );
        assert!(cfg.user_namespaces);
    }

    #[test]
    fn bad_values_are_errors() {
        for bad in [
            ("WORKER_KUBE_IMAGE_PULL_POLICY", "Sometimes"),
            ("WORKER_KUBE_USER_NAMESPACES", "yes"),
            // A key without the hosts it may be used with.
            ("WORKER_GIT_SSH_KEY", "/etc/aurcache/ssh/ssh-privatekey"),
        ] {
            let mut vars = AGENT.to_vec();
            vars.push(bad);
            assert!(read(&vars).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_pod_template_is_read_and_closed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("template.json");
        let write = |json: &str| std::fs::write(&path, json).unwrap();
        let with_template = |path: &std::path::Path| {
            let mut vars = AGENT.to_vec();
            vars.push(("WORKER_KUBE_POD_TEMPLATE", path.to_str().unwrap()));
            read(&vars)
        };

        write(
            r#"{"nodeSelector": {"pool": "builds"}, "priorityClassName": "low",
                "tolerations": [{"key": "builds", "operator": "Exists"}],
                "labels": {"team": "a"}, "annotations": {"x": "y"}}"#,
        );
        let extras = with_template(&path).unwrap().extras;
        assert_eq!(extras.priority_class_name.as_deref(), Some("low"));
        assert_eq!(extras.node_selector.unwrap()["pool"], "builds");
        assert_eq!(extras.tolerations.unwrap().len(), 1);

        for refused in [
            r#"{"volumes": []}"#,
            r#"{"containers": []}"#,
            r#"{"securityContext": {}}"#,
            r#"{"hostNetwork": true}"#,
        ] {
            write(refused);
            assert!(with_template(&path).is_err(), "{refused}");
        }
        assert!(with_template(&dir.path().join("missing.json")).is_err());
    }

    #[test]
    fn limits_come_from_the_settings_and_zero_is_none() {
        let set = declared().with_snapshot(&snapshot(&[
            ("build_memory_max", "8G"),
            ("build_cpus", "1.5"),
            ("build_disk_max", "30G"),
            ("schedule_timeout", "2m"),
            ("keyserver", "hkps://k.example"),
        ]));
        let tunables = Tunables::from_settings(&set);
        assert_eq!(tunables.memory, Some(8 << 30));
        assert_eq!(tunables.cpus, Some(1.5));
        assert_eq!(tunables.disk, Some(30 << 30));
        assert_eq!(tunables.schedule_timeout, Duration::from_secs(120));
        assert_eq!(tunables.keyserver, "hkps://k.example");

        let zero = declared().with_snapshot(&snapshot(&[
            ("build_memory_max", "0"),
            ("build_cpus", "0"),
            ("build_disk_max", "0"),
        ]));
        let tunables = Tunables::from_settings(&zero);
        assert_eq!(tunables.memory, None);
        assert_eq!(tunables.cpus, None);
        assert_eq!(tunables.disk, None);
    }

    #[test]
    fn new_settings_replace_only_the_tunables() {
        let cfg = read(&AGENT).unwrap();
        let set = declared().with_snapshot(&snapshot(&[("build_cpus", "4")]));
        let next = cfg.with_settings(&set);
        assert_eq!(next.tunables.cpus, Some(4.0));
        assert_eq!(
            Config {
                tunables: cfg.tunables.clone(),
                ..next
            },
            cfg
        );
    }
}
