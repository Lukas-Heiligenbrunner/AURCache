//! What each worker protocol's messages look like on the wire, held as
//! fixtures, so a change to the types that would break a peer speaking one of
//! them fails here instead of in production.
//!
//! The types are shared by every protocol and keep evolving (see the module
//! documentation of `aurcache_common::worker`), so nothing else stops an edit
//! from renaming a field, dropping one, making one required or changing its
//! type. For every sample in `fixtures/worker-protocol/<protocol>/`, the test
//! requires that:
//!
//! - it still decodes, so a message from an older peer is still read; and
//! - encoding what it decoded still contains all of it, so a message to an
//!   older peer still carries everything that peer reads.
//!
//! Adding an optional field passes both, as it should. A new variant in an
//! enum a peer receives cannot be caught this way, since no old sample holds
//! it; that one is down to the rule.
//!
//! `registration/` holds `RegisterRequest` and `RegisterStatus`, which are
//! exchanged before any protocol is agreed and so belong to none.
//!
//! `AURCACHE_BLESS_PROTOCOL=1` writes the samples below as the fixtures, for
//! adding a message or a protocol. A fixture already committed for a protocol
//! is never rewritten to make this pass: that is the break it exists to catch.

use aurcache_common::api::activity::Severity;
use aurcache_common::api::builds::{DiskUsage, KeptBuild};
use aurcache_common::api::worker::ApprovalStatus;
use aurcache_common::worker::{
    BuildOutcome, ClaimRequest, CompleteReport, Heartbeat, HeartbeatResponse, JobDescriptor,
    JobStatus, JobVcsSource, MirrorlistOffer, MirrorlistPreference, PROTOCOLS, ProtocolRange,
    RegisterRequest, RegisterStatus, WorkerLogReport,
};
use aurcache_common::worker_config::{
    Applies, ConfigSnapshot, EffectiveConfig, EffectiveSetting, EffectiveSource, SettingDecl,
    SettingStatus, ValueKind,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn fixtures(dir: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/worker-protocol")
        .join(dir)
}

/// Check, or with `AURCACHE_BLESS_PROTOCOL` write, the fixture `name` in `dir`
/// against the type `T`.
fn check<T: Serialize + DeserializeOwned>(dir: &str, name: &str, samples: &[T]) {
    let path = fixtures(dir).join(format!("{name}.json"));
    if std::env::var_os("AURCACHE_BLESS_PROTOCOL").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let json = serde_json::to_string_pretty(samples).unwrap();
        std::fs::write(&path, json + "\n").unwrap();
        return;
    }
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{}: {e}; bless it once it is right", path.display()));
    let fixture: Vec<Value> = serde_json::from_str(&text).unwrap();
    for (i, sample) in fixture.into_iter().enumerate() {
        let decoded: T = serde_json::from_value(sample.clone()).unwrap_or_else(|e| {
            panic!("{} #{i} no longer decodes: {e}", path.display());
        });
        let encoded = serde_json::to_value(&decoded).unwrap();
        if let Some(missing) = missing(&sample, &encoded, "") {
            panic!("{} #{i}: encoding it again loses {missing}", path.display());
        }
    }
}

/// The first place `want` holds something `got` does not, if any.
///
/// Objects may have gained keys; everything else must match exactly.
fn missing(want: &Value, got: &Value, at: &str) -> Option<String> {
    match (want, got) {
        (Value::Object(want), Value::Object(got)) => want.iter().find_map(|(key, value)| {
            let at = format!("{at}.{key}");
            match got.get(key) {
                Some(found) => missing(value, found, &at),
                None => Some(at),
            }
        }),
        (Value::Array(want), Value::Array(got)) if want.len() == got.len() => want
            .iter()
            .zip(got)
            .enumerate()
            .find_map(|(i, (w, g))| missing(w, g, &format!("{at}[{i}]"))),
        _ if want == got => None,
        _ => Some(format!("{at} (was {want}, now {got})")),
    }
}

fn setting_decl() -> SettingDecl {
    SettingDecl {
        key: "max_disk".to_string(),
        kind: ValueKind::Size,
        description: "Disk a build may use".to_string(),
        category: "Limits".to_string(),
        default: Some("50G".to_string()),
        env_var: Some("WORKER_DISK_MAX".to_string()),
        applies: Applies::NextJob,
    }
}

fn effective_config() -> EffectiveConfig {
    EffectiveConfig {
        received_revision: Some("3f2a".to_string()),
        settings: BTreeMap::from([(
            "max_disk".to_string(),
            EffectiveSetting {
                value: Some("100G".to_string()),
                source: EffectiveSource::Server,
                status: SettingStatus::Applied,
                reason: Some("set on the server".to_string()),
            },
        )]),
    }
}

fn job_descriptor(mirrorlist: MirrorlistOffer) -> JobDescriptor {
    JobDescriptor {
        build_id: 42,
        pkgbase: "hello".to_string(),
        arch: "x86_64".to_string(),
        build_flags: vec!["--noconfirm".to_string()],
        persistent_builddir: true,
        makepkg_conf: "MAKEFLAGS=\"-j8\"\n".to_string(),
        pacman_conf: "[options]\n".to_string(),
        mirrorlist,
        pgp_keys: vec!["ABCDEF0123456789".to_string()],
        vcs_sources: vec![JobVcsSource {
            source_url: "git+https://example.invalid/hello.git".to_string(),
            dir: "hello".to_string(),
            git_ref: "0123abcd".to_string(),
        }],
        packages: vec!["hello".to_string(), "hello-docs".to_string()],
        version: "2.12.1-2".to_string(),
    }
}

fn complete_report(outcome: BuildOutcome) -> CompleteReport {
    CompleteReport {
        outcome,
        exit_code: Some(0),
        reason: Some("makepkg exited with 0".to_string()),
        peak_memory_bytes: Some(1_073_741_824),
        disk_usage: Some(DiskUsage {
            chroot: Some(3_221_225_472),
            workdir: Some(41_943_040),
            sources: Some(1_048_576),
            build_tree: Some(20_971_520),
        }),
        vcs_commits: BTreeMap::from([(
            "git+https://example.invalid/hello.git".to_string(),
            "0123abcd".to_string(),
        )]),
        kept: Some(KeptBuild {
            path: "/var/lib/aurcache-chroot/kept-42".to_string(),
            until: 1_800_000_000,
            tree: Some("/var/lib/aurcache-chroot/kept-42.build".to_string()),
        }),
    }
}

#[test]
fn protocol_1_messages_still_decode() {
    check(
        "1",
        "claim_request",
        &[
            ClaimRequest {
                mirrorlist: MirrorlistPreference::Local,
            },
            ClaimRequest {
                mirrorlist: MirrorlistPreference::Server {
                    checksums: BTreeMap::from([("x86_64".to_string(), "9b1c".to_string())]),
                },
            },
        ],
    );
    check(
        "1",
        "job_descriptor",
        &[
            job_descriptor(MirrorlistOffer::Send {
                content: "Server = https://mirror.invalid/$repo/os/$arch\n".to_string(),
                checksum: "9b1c".to_string(),
            }),
            job_descriptor(MirrorlistOffer::Keep),
            job_descriptor(MirrorlistOffer::Nothing),
        ],
    );
    check(
        "1",
        "heartbeat",
        &[Heartbeat {
            active_build_ids: vec![42, 43],
            version: "0.6.0".to_string(),
            effective: Some(effective_config()),
            received_revision: Some("3f2a".to_string()),
        }],
    );
    check(
        "1",
        "heartbeat_response",
        &[HeartbeatResponse {
            cancel: vec![43],
            config: Some(ConfigSnapshot {
                revision: "3f2a".to_string(),
                settings: BTreeMap::from([("max_disk".to_string(), "100G".to_string())]),
            }),
        }],
    );
    check(
        "1",
        "complete_report",
        &[
            complete_report(BuildOutcome::Succeeded),
            complete_report(BuildOutcome::Failed),
            complete_report(BuildOutcome::Canceled),
        ],
    );
    check(
        "1",
        "job_status",
        &[JobStatus {
            cancel_requested: true,
        }],
    );
    check(
        "1",
        "worker_log_report",
        &[WorkerLogReport {
            severity: Severity::Warning,
            build_id: Some(42),
            message: "the chroot needed a fallback".to_string(),
        }],
    );
}

#[test]
fn registration_messages_still_decode() {
    check(
        "registration",
        "register_request",
        &[RegisterRequest {
            protocols: ProtocolRange { min: 1, max: 1 },
            name: "builder-01".to_string(),
            native_arches: vec!["x86_64".to_string()],
            emulated_arches: vec!["aarch64".to_string()],
            version: "0.6.0".to_string(),
            kind: "chroot".to_string(),
            csr_pem: "-----BEGIN CERTIFICATE REQUEST-----\n...\n".to_string(),
            enrollment_token: Some("token".to_string()),
            packages: vec!["chromium".to_string()],
            priority: 10,
            concurrency: 2,
            settings: vec![setting_decl()],
        }],
    );
    check(
        "registration",
        "register_status",
        &[RegisterStatus {
            status: ApprovalStatus::Approved,
            signed_cert: Some("-----BEGIN CERTIFICATE-----\n...\n".to_string()),
            ca_cert: Some("-----BEGIN CERTIFICATE-----\n...\n".to_string()),
            repo_template: "[aurcache]\nServer = http://%AURCACHE_HOST%:8081/$arch\n".to_string(),
            protocol: Some(1),
        }],
    );
}

/// The fixtures cover every protocol this crate speaks.
#[test]
fn every_spoken_protocol_has_fixtures() {
    for protocol in PROTOCOLS.min..=PROTOCOLS.max {
        assert!(
            fixtures(&protocol.to_string()).is_dir(),
            "no fixtures for worker protocol {protocol}"
        );
    }
}
