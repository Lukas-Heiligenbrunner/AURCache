//! Hermetic fake-worker protocol test (tier-2, `fake-worker-test`).
//!
//! Boots the real remote-worker mTLS protocol listener in-process
//! (`aurcache_api::init::init_worker_api`) against an in-memory database and a
//! throwaway internal CA, then drives a synthetic worker through the full wire
//! protocol using the production [`aurcache_worker`] client:
//!
//!   enroll (token auto-approve) -> claim -> upload artifact -> complete.
//!
//! It asserts the server wired the pieces together correctly (mTLS cert ->
//! approved worker mapping, claim -> `JobDescriptor`, artifact staging, and
//! `complete{success}` -> publishing + `files` table) and that the safety
//! rails fire (wrong artifact name rejected, revoked worker refused).
//!
//! It is fully offline: the seeded package points at a nonexistent local git
//! path so the server's best-effort `.SRCINFO` fetch fails instantly (empty
//! `pgp_keys`) instead of reaching the network, and the worker uploads a
//! hand-crafted minimal `*.pkg.tar.zst` rather than building one. The real
//! chroot build + source download are covered by the tier-3 e2e
//! (`scripts/test-e2e.sh`).

use std::io::Write;
use std::time::Duration;

use aurcache_common::build_state::{BuildState, EndReason};
use aurcache_common::settings::Setting;
use aurcache_common::worker::{BuildOutcome, ClaimRequest, CompleteReport, MirrorlistPreference};
use aurcache_db::builds;
use aurcache_db::files;
use aurcache_db::helpers::worker_store;
use aurcache_db::migration::Migrator;
use aurcache_utils::settings;
use aurcache_worker_core::client::{WorkerClient, fetch_and_pin_ca};
use aurcache_worker_core::config::CoreConfig;
use aurcache_worker_core::enroll::ensure_enrolled;
use aurcache_worker_core::identity::Identity;
use sea_orm::{
    ColumnTrait, ConnectionTrait, Database, DatabaseConnection, EntityTrait, QueryFilter,
};
use sea_orm_migration::MigratorTrait;

/// Craft a minimal but structurally valid `*.pkg.tar.zst`: a zstd-compressed
/// tar containing a single `.PKGINFO` with the fields `repo_add` requires
/// (`pkgname` + `pkgver` non-empty). Returned as (filename, bytes).
fn make_pkg(name: &str, ver: &str) -> (String, Vec<u8>) {
    let pkginfo = format!(
        "pkgname = {name}\npkgbase = {name}\npkgver = {ver}\npkgdesc = fake test package\n\
         url = https://example.invalid\nbuilddate = 0\npackager = test <test@localhost>\n\
         size = 0\narch = x86_64\n"
    );

    let mut tar = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_path(".PKGINFO").unwrap();
    header.set_size(pkginfo.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    tar.append(&header, pkginfo.as_bytes()).unwrap();
    let tar_bytes = tar.into_inner().unwrap();

    let mut enc = zstd::Encoder::new(Vec::new(), 0).unwrap();
    enc.write_all(&tar_bytes).unwrap();
    let bytes = enc.finish().unwrap();

    (format!("{name}-{ver}-x86_64.pkg.tar.zst"), bytes)
}

fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

async fn seed_build(db: &DatabaseConnection, id: i32, platform: &str, start: i64) {
    // Nonexistent local git path -> the server's best-effort `.SRCINFO` fetch
    // during `claim` fails instantly (offline, empty pgp_keys) rather than
    // hitting the AUR.
    let source =
        r#"{"type":"git","url":"/nonexistent-aurcache-fake-worker","ref":"HEAD","subfolder":""}"#;
    db.execute_unprepared(&format!(
        "INSERT INTO packages (id, name, build_flags, source_data, platforms) \
         VALUES ({id}, 'p{id}', '', '{source}', '{platform}')"
    ))
    .await
    .unwrap();
    db.execute_unprepared(&format!(
        "INSERT INTO builds (id, pkg_id, status, start_time, platform, version) \
         VALUES ({id}, {id}, 3, {start}, '{platform}', '1.0')"
    ))
    .await
    .unwrap();
}

/// The build's status once it is no longer being published.
async fn settled_status(db: &DatabaseConnection, id: i32) -> BuildState {
    for _ in 0..100 {
        let status = build_status(db, id).await;
        if status != BuildState::Publishing {
            return status;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("build {id} is still publishing");
}

async fn build_status(db: &DatabaseConnection, id: i32) -> BuildState {
    builds::Entity::find_by_id(id)
        .one(db)
        .await
        .unwrap()
        .unwrap()
        .status
}

#[tokio::test]
async fn fake_worker_protocol_roundtrip() {
    let tmp = tempfile::tempdir().unwrap();
    let ca_dir = tmp.path().join("ca");
    let worker_data = tmp.path().join("worker");
    std::env::set_current_dir(tmp.path()).unwrap();

    let port = free_port();
    let token = "s3cr3t-enroll-token";

    // SAFETY: single-threaded setup phase before any worker/server threads read
    // these; each `tests/*.rs` integration test is its own process so there is
    // no cross-test env contention.
    unsafe {
        std::env::set_var("AURCACHE_WORKER_PORT", port.to_string());
        std::env::set_var("AURCACHE_TLS_SANS", "localhost");
        std::env::set_var("AURCACHE_ENROLLMENT_TOKEN", token);
        std::env::remove_var("AURCACHE_ENROLLMENT_DIR");
        std::env::remove_var("AURCACHE_PREAPPROVED_WORKERS");
        // Worker config:
        std::env::set_var("AURCACHE_URL", format!("https://localhost:{port}"));
        std::env::set_var("WORKER_DATA_DIR", worker_data.display().to_string());
        std::env::set_var("WORKER_ARCHES", "x86_64");
        std::env::set_var("WORKER_NAME", "fake-worker");
    }

    // In-memory DB with the full schema, seeded with two enqueued x86_64 builds.
    let db = Database::connect("sqlite::memory:").await.unwrap();
    Migrator::up(&db, None).await.unwrap();
    seed_build(&db, 1, "x86_64", 1000).await;
    seed_build(&db, 2, "x86_64", 2000).await;

    // Boot the real worker protocol listener (HTTPS + optional mTLS).
    let ca = aurcache_ca::Ca::load_or_create(&ca_dir).unwrap();
    let repo_root = tmp.path().join("repo");
    let _server = aurcache_api::init::init_worker_api(
        db.clone(),
        ca,
        std::sync::Arc::new(aurcache_utils::snapshot::SnapshotStore::new()),
        std::sync::Arc::new(aurcache_utils::repository::Repository::new(&repo_root)),
        aurcache_activitylog::activity_utils::ActivityLog::discarding(),
    );

    // Wait for the TLS listener to accept and serve the CA.
    let base = format!("https://localhost:{port}");
    let mut up = false;
    for _ in 0..80 {
        if fetch_and_pin_ca(&base, None).await.is_ok() {
            up = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert!(up, "worker protocol listener did not come up on :{port}");

    // Enroll a synthetic worker with the production client + enrollment flow.
    // The shared enrollment token makes the server auto-approve it.
    let cfg = CoreConfig::from_env();
    let identity = Identity::load_or_create(&cfg.data_dir).unwrap();
    let client: WorkerClient = ensure_enrolled(&cfg, &identity, "chroot")
        .await
        .expect("worker should enroll and be auto-approved via token");

    // --- The worker's declaration reaches the server over the real wire, and
    // the heartbeat carries what those settings resolved to.
    //
    // Both are what the Workers page reads, and both travel as JSON the server
    // stores without interpreting -- so a shape that serializes on the worker
    // and does not deserialize on the server would show up nowhere but here.
    {
        let stored = worker_store::find_worker_by_fingerprint(&db, &identity.fingerprint)
            .await
            .unwrap()
            .expect("the enrolled worker should have a row");
        let declared: Vec<aurcache_common::worker_config::SettingDecl> =
            serde_json::from_str(&stored.settings_declaration)
                .expect("the stored declaration should read back");
        assert!(
            declared.iter().any(|decl| decl.key == "concurrency"),
            "the protocol settings should be declared: {declared:?}"
        );
        assert!(
            stored.effective_config.is_none(),
            "nothing is reported until the first heartbeat"
        );

        let first = client
            .heartbeat(&aurcache_common::worker::Heartbeat {
                active_build_ids: vec![],
                version: "test".to_string(),
                effective: Some(cfg.settings.effective()),
                received_revision: None,
            })
            .await
            .expect("heartbeat should be accepted");

        let stored = worker_store::find_worker_by_fingerprint(&db, &identity.fingerprint)
            .await
            .unwrap()
            .expect("the worker row should still be there");
        let report: aurcache_common::worker_config::EffectiveConfig = serde_json::from_str(
            stored
                .effective_config
                .as_deref()
                .expect("the heartbeat should have stored a report"),
        )
        .expect("the stored report should read back");
        assert!(
            report.settings.contains_key("concurrency"),
            "the report should cover the declared settings: {report:?}"
        );

        // --- Values set on the server reach the worker over the heartbeat.
        //
        // A worker holding nothing is sent the snapshot even when nothing is
        // set: "no values" is a statement it has to have received to know.
        let empty = first
            .config
            .expect("a worker that declares settings holds no snapshot yet");
        assert!(empty.settings.is_empty());

        // What the PATCH endpoint does once it has checked the values.
        worker_store::save_worker_settings(
            &db,
            stored.id,
            &std::collections::BTreeMap::from([
                ("concurrency".to_string(), Some("3".to_string())),
                ("build_timeout".to_string(), Some("6h".to_string())),
            ]),
        )
        .await
        .unwrap();

        // Still holding the empty one, so the new one is sent.
        let delivered = client
            .heartbeat(&aurcache_common::worker::Heartbeat {
                active_build_ids: vec![],
                version: "test".to_string(),
                effective: None,
                received_revision: Some(empty.revision.clone()),
            })
            .await
            .unwrap()
            .config
            .expect("a save should be delivered on the next heartbeat");
        assert_ne!(delivered.revision, empty.revision);
        assert_eq!(delivered.settings["concurrency"], "3");

        // Taken in the way the runner takes it, and reported back.
        let next = cfg.with_settings(cfg.settings.with_snapshot(&delivered));
        assert_eq!(next.concurrency, 3);
        assert_eq!(next.build_timeout, 6 * 60 * 60);
        let held = client
            .heartbeat(&aurcache_common::worker::Heartbeat {
                active_build_ids: vec![],
                version: "test".to_string(),
                effective: Some(next.settings.effective()),
                received_revision: Some(delivered.revision.clone()),
            })
            .await
            .unwrap();
        assert!(
            held.config.is_none(),
            "a snapshot the worker holds was resent"
        );
        let stored = worker_store::find_worker(&db, stored.id)
            .await
            .unwrap()
            .unwrap();
        let report: aurcache_common::worker_config::EffectiveConfig =
            serde_json::from_str(stored.effective_config.as_deref().unwrap()).unwrap();
        assert_eq!(
            report.received_revision.as_deref(),
            Some(delivered.revision.as_str())
        );
        assert_eq!(
            report.settings["concurrency"].source,
            aurcache_common::worker_config::EffectiveSource::Server
        );

        // Concurrency is also what the server schedules by, so the worker
        // registers again with it -- and the row the scheduler reads follows.
        client
            .register(&aurcache_worker_core::enroll::register_request(
                &next,
                identity.generate_csr(&next.name).unwrap(),
                "chroot",
            ))
            .await
            .expect("registering again while enrolled should be accepted");
        let stored = worker_store::find_worker(&db, stored.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.concurrency, 3);
        assert_eq!(
            stored.status,
            aurcache_common::api::worker::ApprovalStatus::Approved,
            "registering again must not touch approval"
        );
    }

    let claim_req = ClaimRequest {
        // Holds nothing, so the server sends any mirrorlist in full.
        mirrorlist: MirrorlistPreference::Server {
            checksums: Default::default(),
        },
    };

    // --- Happy path: claim build 1 (oldest), upload a good artifact, complete.
    let job = client
        .claim(&claim_req)
        .await
        .unwrap()
        .expect("a job should be claimable");
    assert_eq!(job.build_id, 1);
    assert_eq!(job.pkgbase, "p1");
    assert_eq!(job.arch, "x86_64");
    assert!(job.pgp_keys.is_empty(), "offline source -> no pgp keys");

    client.append_log(1, "fake build starting\n").await.unwrap();

    // --- The artifact limit is the package's setting, and an artifact over it
    // is refused -- before a byte is read when the size is declared, and at the
    // limit when it is not -- with the server's reason, and nothing staged.
    settings::write(
        &db,
        [settings::Change {
            setting: Setting::MaxArtifactSize,
            pkg_id: Some(1),
            value: Some("1K".to_string()),
        }],
    )
    .await
    .unwrap();
    let oversized = vec![0u8; 64 * 1024];
    for declared in [Some(oversized.len() as u64), None] {
        let refused = client
            .upload_artifact(
                1,
                "p1-1.0-1-x86_64.pkg.tar.zst",
                std::io::Cursor::new(oversized.clone()),
                declared,
            )
            .await
            .expect_err("an artifact over the package's limit must be refused");
        let message = format!("{refused:#}");
        assert!(
            message.contains("413")
                && message.contains("max_artifact_size")
                && message.contains("1K"),
            "the refusal should name the limit ({declared:?}): {message}"
        );
        assert!(
            !repo_root
                .join(".staging")
                .join("1")
                .join("p1-1.0-1-x86_64.pkg.tar.zst")
                .exists(),
            "a refused artifact left a partial file behind ({declared:?})"
        );
    }
    settings::write(
        &db,
        [settings::Change {
            setting: Setting::MaxArtifactSize,
            pkg_id: Some(1),
            value: None,
        }],
    )
    .await
    .unwrap();

    let (fname, bytes) = make_pkg("p1", "1.0-1");
    client
        .upload_artifact(1, &fname, std::io::Cursor::new(bytes), None)
        .await
        .unwrap();
    client
        .complete(
            1,
            &CompleteReport {
                outcome: BuildOutcome::Succeeded,
                exit_code: Some(0),
                reason: None,
                peak_memory_bytes: Some(512 * 1024 * 1024),
                disk_usage: None,
                vcs_commits: Default::default(),
                kept: None,
            },
        )
        .await
        .expect("complete{success} should be accepted");

    // Accepted, then published in the background.
    assert_eq!(settled_status(&db, 1).await, BuildState::Successful);
    let repo_db = repo_root.join("x86_64").join("repo.db.tar.gz");
    assert!(repo_db.exists(), "repo db should be written at {repo_db:?}");
    let file_rows = files::Entity::find()
        .filter(files::Column::PackageId.eq(1))
        .all(&db)
        .await
        .unwrap();
    assert!(!file_rows.is_empty(), "a files row should be recorded");
    assert!(
        !repo_root.join(".staging").join("1").exists(),
        "the staging directory goes once the build is published"
    );

    // A repeated completion -- its first answer lost -- is acknowledged, and
    // changes nothing.
    client
        .complete(
            1,
            &CompleteReport {
                outcome: BuildOutcome::Succeeded,
                exit_code: Some(0),
                reason: None,
                peak_memory_bytes: None,
                disk_usage: None,
                vcs_commits: Default::default(),
                kept: None,
            },
        )
        .await
        .expect("a repeated completion is acknowledged");
    assert_eq!(build_status(&db, 1).await, BuildState::Successful);

    // --- Safety rail: a wrong-named artifact is never published.
    let job2 = client
        .claim(&claim_req)
        .await
        .unwrap()
        .expect("second job claimable");
    assert_eq!(job2.build_id, 2);

    let (bad_name, bad_bytes) = make_pkg("evil", "9.9-1");
    client
        .upload_artifact(2, &bad_name, std::io::Cursor::new(bad_bytes), None)
        .await
        .unwrap();
    client
        .complete(
            2,
            &CompleteReport {
                outcome: BuildOutcome::Succeeded,
                exit_code: Some(0),
                reason: None,
                peak_memory_bytes: Some(512 * 1024 * 1024),
                disk_usage: None,
                vcs_commits: Default::default(),
                kept: None,
            },
        )
        .await
        .expect("the worker's part is done either way");
    // Refused at publishing, which is the server's: the build fails, and
    // nothing of it reaches the repository.
    assert_eq!(settled_status(&db, 2).await, BuildState::Failed);
    let evil_rows = files::Entity::find()
        .filter(files::Column::PackageId.eq(2))
        .all(&db)
        .await
        .unwrap();
    assert!(evil_rows.is_empty(), "a wrong-named artifact was published");
    assert!(!repo_root.join("x86_64").join(&bad_name).exists());

    // --- An empty queue is "nothing to do" (204), not an error: an idle
    // worker asks every few seconds, and a 404 there filled the server's log.
    assert!(
        client.claim(&claim_req).await.unwrap().is_none(),
        "nothing is left to claim"
    );

    // --- A build its worker could not finish is queued again, rather than
    // failed for good.
    seed_build(&db, 3, "x86_64", 3000).await;
    let job3 = client
        .claim(&claim_req)
        .await
        .unwrap()
        .expect("third job claimable");
    assert_eq!(job3.build_id, 3);
    let interrupted = CompleteReport {
        outcome: BuildOutcome::Interrupted,
        reason: Some("downloading source: connection reset".to_string()),
        ..CompleteReport::default()
    };
    client
        .complete(3, &interrupted)
        .await
        .expect("an interruption is accepted");
    let row = builds::Entity::find_by_id(3)
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.status, BuildState::Failed);
    assert_eq!(row.end_reason, Some(EndReason::Interrupted));
    let retries = builds::Entity::find()
        .filter(builds::Column::PkgId.eq(3))
        .filter(builds::Column::Status.eq(BuildState::Enqueued))
        .all(&db)
        .await
        .unwrap();
    assert_eq!(retries.len(), 1, "one fresh attempt is queued");
    // Its answer lost, the worker reports it again: acknowledged, no change.
    client
        .complete(3, &interrupted)
        .await
        .expect("a repeated interruption is acknowledged");
    let retries = builds::Entity::find()
        .filter(builds::Column::PkgId.eq(3))
        .filter(builds::Column::Status.eq(BuildState::Enqueued))
        .all(&db)
        .await
        .unwrap();
    assert_eq!(retries.len(), 1, "and no second one");

    // --- Safety rail: a revoked worker is refused at the mTLS auth guard.
    let workers = worker_store::list_workers(&db).await.unwrap();
    let worker_id = workers.first().expect("one enrolled worker").id;
    worker_store::revoke_worker(&db, worker_id, 3)
        .await
        .unwrap()
        .expect("the worker exists");
    let after_revoke = client.claim(&claim_req).await;
    assert!(
        after_revoke.is_err(),
        "a revoked worker's cert must be rejected: {after_revoke:?}"
    );
}
