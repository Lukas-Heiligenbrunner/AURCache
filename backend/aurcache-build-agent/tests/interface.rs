//! The agent's interface as JSON, held as fixtures.
//!
//! The agent and the worker that runs it may be different builds: a worker
//! can be handed another agent (`WORKER_BUILD_AGENT`), and the Kubernetes
//! worker's comes from an image someone named. So the spec a worker writes,
//! the report the agent writes and the hand-back it prints are frozen for
//! each interface number under `fixtures/interface-<n>/`, and each fixture
//! must decode and encode back to exactly the same JSON.
//!
//! Within an interface these types only gain optional fields, and a new one
//! is `#[serde(default, skip_serializing_if = "Option::is_none")]`, which
//! leaves every fixture as it is. Anything else changes a fixture, and is the
//! next interface.
//!
//! `AURCACHE_BLESS_INTERFACE=1` writes the samples below as the fixtures, for
//! a new interface. A fixture already committed is never rewritten to make
//! this pass: that is the break it exists to catch.

use aurcache_build_agent::report::{Artifact, BuildReport, Ended, Handback, SetupStep};
use aurcache_build_agent::spec::{BuildSpec, INTERFACE, SshAccess};
use aurcache_common::worker::JobVcsSource;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(format!("interface-{INTERFACE}"))
        .join(format!("{name}.json"))
}

/// Check, or with `AURCACHE_BLESS_INTERFACE` write, the fixture `name`
/// against the type `T`, returning what it holds.
fn check<T: Serialize + DeserializeOwned>(name: &str, samples: &[T]) -> Vec<Value> {
    let path = fixture(name);
    if std::env::var_os("AURCACHE_BLESS_INTERFACE").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let json = serde_json::to_string_pretty(samples).unwrap();
        std::fs::write(&path, json + "\n").unwrap();
    }
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{}: {e}; bless it once it is right", path.display()));
    let fixture: Vec<Value> = serde_json::from_str(&text).unwrap();
    assert!(!fixture.is_empty(), "{} holds no sample", path.display());
    for (i, sample) in fixture.iter().enumerate() {
        let decoded: T = serde_json::from_value(sample.clone())
            .unwrap_or_else(|e| panic!("{} #{i} no longer decodes: {e}", path.display()));
        let encoded = serde_json::to_value(&decoded).unwrap();
        assert_eq!(
            &encoded,
            sample,
            "{} #{i} does not encode back to the same JSON",
            path.display()
        );
    }
    fixture
}

fn report(ended: Ended) -> BuildReport {
    BuildReport {
        interface: INTERFACE,
        ended,
        peak_memory_bytes: Some(121_000_000),
        oom_kills: Some(0),
        disk_bytes: Some(52_428_800),
        vcs_commits: BTreeMap::from([(
            "git+https://example.test/repo.git#branch=main".to_string(),
            "6e481d6bf0a69f8c9bd2866eb491e1e4e9b0717f".to_string(),
        )]),
    }
}

fn specs() -> Vec<BuildSpec> {
    let full = BuildSpec {
        interface: INTERFACE,
        pkgbase: "hello".to_string(),
        arch: "x86_64".to_string(),
        build_flags: vec!["--skippgpcheck".to_string()],
        makepkg_conf: "OPTIONS=(!debug)\n".to_string(),
        pacman_conf: "[options]\nArchitecture = auto\n\n[core]\nInclude = /etc/pacman.d/mirrorlist\n\n[aurcache]\nSigLevel = Optional TrustAll\nServer = http://aurcache.test/x86_64\n".to_string(),
        pacman_upgrade_conf: "[options]\nArchitecture = auto\n\n[core]\nInclude = /etc/pacman.d/mirrorlist\n".to_string(),
        mirrorlist: Some("Server = https://mirror.test/$repo/os/$arch\n".to_string()),
        pgp_keys: vec!["ABAF11C65A2970B130ABE3C479BE3E4300411886".to_string()],
        keyserver: "hkps://keyserver.ubuntu.com".to_string(),
        vcs_sources: vec![JobVcsSource {
            source_url: "git+https://example.test/repo.git#branch=main".to_string(),
            dir: "repo".to_string(),
            git_ref: "main".to_string(),
        }],
        cpus: Some(2.5),
        scratch: PathBuf::from("/work/scratch"),
        ssh: None,
    };
    let bare = BuildSpec {
        build_flags: Vec::new(),
        makepkg_conf: String::new(),
        mirrorlist: None,
        pgp_keys: Vec::new(),
        vcs_sources: Vec::new(),
        cpus: None,
        scratch: PathBuf::from("/var/lib/aurcache-build"),
        ..full.clone()
    };
    let with_key = BuildSpec {
        ssh: Some(SshAccess {
            known_hosts: "github.com ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl\n".to_string(),
        }),
        ..full.clone()
    };
    vec![full, bare, with_key]
}

fn reports() -> Vec<BuildReport> {
    let mut steps: Vec<BuildReport> = [
        SetupStep::Spec,
        SetupStep::Config,
        SetupStep::Keyring,
        SetupStep::Sync,
        SetupStep::User,
        SetupStep::Source,
        SetupStep::Keys,
        SetupStep::Dependencies,
        SetupStep::Credential,
    ]
    .into_iter()
    .map(|step| BuildReport {
        peak_memory_bytes: None,
        oom_kills: None,
        disk_bytes: None,
        vcs_commits: BTreeMap::new(),
        ..report(Ended::Setup {
            step,
            message: "error: failed retrieving file 'core.db'".to_string(),
        })
    })
    .collect();
    let mut all = vec![
        report(Ended::Exited { code: 0 }),
        report(Ended::Exited { code: 4 }),
        report(Ended::Signaled { signal: 9 }),
    ];
    all.append(&mut steps);
    all
}

fn handbacks() -> Vec<Handback> {
    vec![
        Handback {
            interface: INTERFACE,
            report: Some(report(Ended::Exited { code: 0 })),
            artifacts: vec![
                Artifact {
                    name: "hello-1.0-1-x86_64.pkg.tar.zst".to_string(),
                    size: 4096,
                    sha256: "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"
                        .to_string(),
                },
                Artifact {
                    name: "hello-docs-1.0-1-any.pkg.tar.zst".to_string(),
                    size: 0,
                    sha256: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
                        .to_string(),
                },
            ],
        },
        Handback {
            interface: INTERFACE,
            report: None,
            artifacts: Vec::new(),
        },
    ]
}

#[test]
fn spec() {
    check("spec", &specs());
}

#[test]
fn build_report() {
    check("report", &reports());
}

#[test]
fn handback() {
    check("handback", &handbacks());
}

/// A worker of a later build may add an optional field the agent does not
/// know; the agent reads the spec anyway.
#[test]
fn a_spec_with_an_unknown_field_still_reads() {
    for (i, mut sample) in check("spec", &specs()).into_iter().enumerate() {
        let object = sample.as_object_mut().unwrap();
        object.insert("added_later".to_string(), Value::from("x"));
        if let Some(Value::Object(ssh)) = object.get_mut("ssh") {
            ssh.insert("added_later".to_string(), Value::from(1));
        }
        serde_json::from_value::<BuildSpec>(sample)
            .unwrap_or_else(|e| panic!("spec #{i} with an unknown field: {e}"));
    }
}

/// The interface number is in every document, where the agent and a worker
/// read it before anything else.
#[test]
fn every_document_carries_the_interface() {
    for name in ["spec", "report", "handback"] {
        let text = std::fs::read_to_string(fixture(name)).unwrap();
        let samples: Vec<Value> = serde_json::from_str(&text).unwrap();
        for sample in samples {
            assert_eq!(sample["interface"], Value::from(INTERFACE), "{name}");
        }
    }
}
