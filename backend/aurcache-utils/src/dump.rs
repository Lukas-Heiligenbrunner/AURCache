//! Writing a lite export of an instance's authored state.
//!
//! See `aurcache_common::api::dump` for the format and what it deliberately
//! leaves out. This module is the writer; nothing here decides *what* is
//! authored -- that boundary is the format's, and it is documented there.

use std::collections::BTreeMap;
use std::io::Write;

use aurcache_common::api::dump::{
    CA_CERT_FILE, CA_KEY_FILE, DUMP_SCHEMA_VERSION, DumpManifest, DumpPackage, DumpPackages,
    DumpSecrets, DumpSettings, DumpToken, DumpWorker, MANIFEST_FILE, PACKAGES_FILE, PATCH_DIR,
    SETTINGS_FILE, TOKENS_FILE, WORKERS_FILE,
};
use aurcache_common::api::worker::ApprovalStatus;
use aurcache_db::api_tokens;
use aurcache_db::helpers::time::now_secs;
use aurcache_db::prelude::{ApiTokens, Packages, Settings, WorkerSettings, Workers};
use aurcache_db::{packages, settings, workers};
use flate2::Compression;
use flate2::write::GzEncoder;
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, QueryOrder};

/// Everything a dump carries, before it is packed into an archive.
///
/// Built in one step so the archive writer has nothing to decide and the whole
/// thing can be asserted on in a test without going through a tarball.
pub struct Dump {
    pub manifest: DumpManifest,
    pub packages: DumpPackages,
    pub settings: DumpSettings,
    pub workers: Vec<DumpWorker>,
    /// Patch contents by pkgbase, written as `patches/<pkgbase>.patch`.
    pub patches: BTreeMap<String, String>,
    /// Present only when the dump was taken with secrets.
    pub secrets: Option<DumpSecrets>,
}

/// Read the authored state out of the database.
///
/// `ca_dir` opts the dump into carrying secrets: the CA, the certificates the
/// workers hold, and the API token hashes. `None` -- the default everywhere --
/// produces a dump that is safe to keep beside ordinary backups.
pub async fn build_dump(
    db: &DatabaseConnection,
    aurcache_version: &str,
    ca_dir: Option<&std::path::Path>,
) -> anyhow::Result<Dump> {
    // Ordered by name so two dumps of the same instance are byte-identical and
    // a dump kept in git shows only real changes.
    let package_rows = Packages::find()
        .order_by_asc(packages::Column::Name)
        .all(db)
        .await?;

    let mut packages = DumpPackages::new();
    let mut patches = BTreeMap::new();
    // Settings key on `pkg_id`; the dump keys on pkgbase, so the mapping has to
    // be built here while both are in hand.
    let mut pkgbase_of_id = BTreeMap::new();

    for row in package_rows {
        pkgbase_of_id.insert(row.id, row.name.clone());
        if let Some(patch) = row.patch.as_ref().filter(|patch| !patch.is_empty()) {
            patches.insert(row.name.clone(), patch.clone());
        }
        packages.insert(
            row.name.clone(),
            DumpPackage {
                source_data: row.source_data,
                platforms: row.platforms.names(),
                build_flags: row.build_flags.as_slice().to_vec(),
                directly_requested: row.directly_requested,
                has_patch: patches.contains_key(&row.name),
            },
        );
    }

    let settings = build_settings(db, &pkgbase_of_id).await?;
    let secrets = match ca_dir {
        Some(dir) => Some(build_secrets(db, dir).await?),
        None => None,
    };
    let workers = build_workers(db, secrets.is_some()).await?;

    Ok(Dump {
        manifest: DumpManifest {
            schema_version: DUMP_SCHEMA_VERSION,
            aurcache_version: aurcache_version.to_string(),
            created_at: now_secs(),
            // Derived from what is actually in the dump rather than from what
            // was asked for, so the flag and the file cannot disagree about
            // something this consequential.
            includes_secrets: secrets.is_some(),
        },
        packages,
        settings,
        workers,
        patches,
        secrets,
    })
}

/// Read the CA and the token hashes.
///
/// The CA is read from its files rather than from a loaded `Ca`, deliberately:
/// there is no method anywhere that hands out the private key of a CA already
/// in memory, and this does not add one. Export moves files; so does restore.
async fn build_secrets(
    db: &DatabaseConnection,
    ca_dir: &std::path::Path,
) -> anyhow::Result<DumpSecrets> {
    let ca_dir = ca_dir.to_path_buf();
    let ca = tokio::task::spawn_blocking(move || aurcache_ca::Ca::load(&ca_dir)).await??;
    let ca_cert_pem = ca.ca_cert_pem().to_owned();
    let ca_key_pem = ca.private_key_pem().to_owned();

    let tokens = ApiTokens::find()
        .order_by_asc(api_tokens::Column::Username)
        .all(db)
        .await?
        .into_iter()
        .map(|row| DumpToken {
            username: row.username,
            token_hash: row.token_hash,
        })
        .collect();

    Ok(DumpSecrets {
        ca_cert_pem,
        ca_key_pem,
        tokens,
    })
}

/// Settings, re-keyed from `pkg_id` onto pkgbase.
///
/// A per-package setting whose package is gone is dropped: it names a package
/// the dump does not carry, so a restore would have nowhere to put it.
async fn build_settings(
    db: &DatabaseConnection,
    pkgbase_of_id: &BTreeMap<i32, String>,
) -> anyhow::Result<DumpSettings> {
    let mut dumped = DumpSettings::default();
    let rows = Settings::find()
        .order_by_asc(settings::Column::Key)
        .all(db)
        .await?;

    for row in rows {
        // A setting with no value is the absence of a setting; carrying it
        // would restore a row that means nothing.
        let Some(value) = row.value else { continue };
        match settings::package_of(row.pkg_id) {
            None => {
                dumped.global.insert(row.key, value);
            }
            Some(pkg_id) => {
                if let Some(pkgbase) = pkgbase_of_id.get(&pkg_id) {
                    dumped
                        .packages
                        .entry(pkgbase.clone())
                        .or_default()
                        .insert(row.key, value);
                }
            }
        }
    }
    Ok(dumped)
}

/// Approved workers and their routing.
///
/// Only approved ones: a pending enrolment is a decision nobody has made yet,
/// and restoring it would carry an unanswered question into the new instance.
async fn build_workers(
    db: &DatabaseConnection,
    with_certificates: bool,
) -> anyhow::Result<Vec<DumpWorker>> {
    let rows = Workers::find()
        .filter(workers::Column::Status.eq(ApprovalStatus::Approved))
        .order_by_asc(workers::Column::Name)
        .all(db)
        .await?;

    // One query for the whole fleet rather than one per worker.
    let mut values: BTreeMap<i32, BTreeMap<String, String>> = BTreeMap::new();
    for row in WorkerSettings::find().all(db).await? {
        values
            .entry(row.worker_id)
            .or_default()
            .insert(row.key, row.value);
    }

    Ok(rows
        .into_iter()
        .map(|row| DumpWorker {
            settings: values.remove(&row.id).unwrap_or_default(),
            name: row.name,
            cert_fingerprint: row.cert_fingerprint,
            status: row.status,
            arches: row.arches.into_vec(),
            package_affinity: row.package_affinity.into_vec(),
            priority: row.priority,
            concurrency: row.concurrency,
            // Only alongside the CA that signed them. On their own they would
            // authenticate nobody, and a dump that looked complete while being
            // useless is worse than one that plainly has no certificates.
            signed_cert: with_certificates.then(|| row.signed_cert.clone()).flatten(),
            not_after: with_certificates.then_some(row.not_after).flatten(),
        })
        .collect())
}

/// Pack a dump into the `.tar.gz` the format describes.
pub fn write_archive(dump: &Dump) -> anyhow::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    {
        let mut tar = tar::Builder::new(GzEncoder::new(&mut bytes, Compression::default()));
        append(&mut tar, MANIFEST_FILE, &to_json(&dump.manifest)?)?;
        append(&mut tar, PACKAGES_FILE, &to_json(&dump.packages)?)?;
        append(&mut tar, SETTINGS_FILE, &to_json(&dump.settings)?)?;
        append(&mut tar, WORKERS_FILE, &to_json(&dump.workers)?)?;
        for (pkgbase, patch) in &dump.patches {
            append(&mut tar, &format!("{PATCH_DIR}/{pkgbase}.patch"), patch)?;
        }
        if let Some(secrets) = &dump.secrets {
            append(&mut tar, CA_CERT_FILE, &secrets.ca_cert_pem)?;
            append(&mut tar, CA_KEY_FILE, &secrets.ca_key_pem)?;
            append(&mut tar, TOKENS_FILE, &to_json(&secrets.tokens)?)?;
        }
        // Finish explicitly: dropping the encoder would swallow a compression
        // error.
        tar.into_inner()?.finish()?;
    }
    Ok(bytes)
}

/// Pretty-printed, with a trailing newline: a dump is meant to be read, diffed
/// and edited by hand, and compression makes the extra bytes free.
fn to_json<T: serde::Serialize>(value: &T) -> anyhow::Result<String> {
    Ok(format!("{}\n", serde_json::to_string_pretty(value)?))
}

fn append<W: Write>(tar: &mut tar::Builder<W>, path: &str, content: &str) -> anyhow::Result<()> {
    let mut header = tar::Header::new_gnu();
    header.set_path(path)?;
    header.set_size(content.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    tar.append(&header, content.as_bytes())?;
    Ok(())
}
