//! Reading a dump back in.
//!
//! The order here is the whole design, and it is not the obvious one.
//!
//! Every package row is inserted *before* any dependency is resolved. That
//! removes the question of what order to import in -- there is no order, so a
//! dependency cycle is not a special case -- but on its own it is not enough. A
//! local package satisfies a dependency by its pkgbase, by one of its split
//! package names, or by something it `provides`, and a dump carries only the
//! first: the other two are derived from the PKGBUILD and are deliberately not
//! exported.
//!
//! So a git-sourced package that provides `libfoo` would not be recognised as
//! satisfying a dependency on `libfoo`, and resolution would fall through to
//! the AUR and adopt an AUR package in its place -- silently replacing the
//! source the operator chose. Which is why every package's source is resolved,
//! filling in `provides` and its split package names, *before* the dependency
//! graph is rebuilt. By then every package the dump carried is a complete
//! candidate and nothing can fall through.
//!
//! Restore has to read those sources anyway: `dependencies` is not exported
//! either -- an imported graph could disagree with today's AUR -- so the depends
//! lists have to be rediscovered from the sources regardless.

use aurcache_db::lists::{BuildFlags, Platforms, WorkerList};
use std::collections::{BTreeMap, HashSet};
use std::io::Read;

use aurcache_common::api::dump::{
    CA_CERT_FILE, CA_KEY_FILE, DUMP_SCHEMA_VERSION, DumpManifest, DumpPackage, DumpPackages,
    DumpSecrets, DumpSettings, DumpToken, DumpWorker, ExistingPackagePolicy, MANIFEST_FILE,
    PACKAGES_FILE, PATCH_DIR, RestoreEntry, RestoreOptions, RestoreOutcome, SETTINGS_FILE,
    SecretsPolicy, TOKENS_FILE, WORKERS_FILE,
};
use aurcache_common::build_state::BuildState;
use aurcache_common::settings::{Scope, Setting};
use aurcache_db::prelude::Packages;
use aurcache_db::{packages, settings};
use flate2::read::GzDecoder;
use sea_orm::ActiveValue::Set;
use sea_orm::{ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};
use tokio::sync::mpsc::Sender;

use crate::package::add::{provides_json, split_packages_json};
use crate::repository::Repository;
use crate::services::Services;
use crate::snapshot::SnapshotStore;
use aurcache_activitylog::events::{Event, RestoreStep};

/// A dump that has been read and checked, ready to apply.
#[derive(Clone, Debug)]
pub struct LoadedDump {
    pub manifest: DumpManifest,
    pub packages: DumpPackages,
    pub settings: DumpSettings,
    pub workers: Vec<DumpWorker>,
    pub patches: BTreeMap<String, String>,
    /// Present only when the dump was taken with secrets.
    pub secrets: Option<DumpSecrets>,
}

/// Read and validate a `.tar.gz` dump.
///
/// Everything is checked before anything is applied. An import that fails on
/// package 90 of 200 leaves a database that is neither what it was nor what the
/// dump describes, which is worse than either.
pub fn load_dump(bytes: &[u8]) -> anyhow::Result<LoadedDump> {
    let mut files = BTreeMap::new();
    let mut archive = tar::Archive::new(GzDecoder::new(bytes));
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.to_string_lossy().into_owned();
        let mut content = String::new();
        // A dump holds JSON and patches; anything that is not text is not
        // something this format has ever contained.
        entry
            .read_to_string(&mut content)
            .map_err(|e| anyhow::anyhow!("{path} is not readable as text: {e}"))?;
        files.insert(path, content);
    }

    let manifest: DumpManifest = parse(&files, MANIFEST_FILE)?;
    // Checked before anything else is even parsed: a newer dump may describe
    // things this version has no idea how to apply, and guessing at them is how
    // a restore silently drops what it did not understand.
    if manifest.schema_version > DUMP_SCHEMA_VERSION {
        anyhow::bail!(
            "this dump is version {} but this AURCache understands up to {DUMP_SCHEMA_VERSION}; \
             upgrade AURCache to restore it",
            manifest.schema_version
        );
    }

    let packages: DumpPackages = parse(&files, PACKAGES_FILE)?;
    let settings: DumpSettings = parse(&files, SETTINGS_FILE)?;
    let workers: Vec<DumpWorker> = parse(&files, WORKERS_FILE)?;

    let mut patches = BTreeMap::new();
    for (path, content) in &files {
        if let Some(pkgbase) = path
            .strip_prefix(&format!("{PATCH_DIR}/"))
            .and_then(|name| name.strip_suffix(".patch"))
        {
            // A patch for a package the dump does not carry has nothing to
            // attach to. Refused rather than ignored: it means the archive was
            // assembled wrongly, and silently dropping it loses an edit someone
            // made deliberately.
            if !packages.contains_key(pkgbase) {
                anyhow::bail!("{path} has no matching package in {PACKAGES_FILE}");
            }
            patches.insert(pkgbase.to_string(), content.clone());
        }
    }

    // Settings naming a package the dump does not carry would have nowhere to
    // go, and would quietly not be applied.
    for pkgbase in settings.packages.keys() {
        if !packages.contains_key(pkgbase) {
            anyhow::bail!(
                "{SETTINGS_FILE} configures '{pkgbase}', which is not in {PACKAGES_FILE}"
            );
        }
    }

    for (pkgbase, package) in &packages {
        if package.platforms.is_empty() {
            anyhow::bail!("'{pkgbase}' lists no platforms, so nothing could build it");
        }
        for name in &package.platforms {
            name.parse::<pacman_mirrors::platforms::Platform>()
                .map_err(|e| anyhow::anyhow!("'{pkgbase}': {e}"))?;
        }
    }

    let secrets = load_secrets(&files, &workers)?;
    // The flag exists so a reader can tell what it is holding before unpacking
    // it. One that disagrees with the contents is a dump that has been edited,
    // and the safe reading of "the manifest says no secrets" is not one worth
    // guessing at.
    if manifest.includes_secrets != secrets.is_some() {
        anyhow::bail!(
            "the manifest says includes_secrets={} but the archive {} secrets",
            manifest.includes_secrets,
            if secrets.is_some() {
                "carries"
            } else {
                "does not carry"
            }
        );
    }

    Ok(LoadedDump {
        manifest,
        packages,
        settings,
        workers,
        patches,
        secrets,
    })
}

/// The CA, worker certificates and token hashes, if the archive carries them.
///
/// They travel as one unit. A signed certificate means nothing without the CA
/// that signed it, so a dump holding certificates and no CA would look complete
/// and authenticate nobody -- and half a CA is not a CA at all.
fn load_secrets(
    files: &BTreeMap<String, String>,
    workers: &[DumpWorker],
) -> anyhow::Result<Option<DumpSecrets>> {
    let cert = files.get(CA_CERT_FILE);
    let key = files.get(CA_KEY_FILE);

    let (ca_cert_pem, ca_key_pem) = match (cert, key) {
        (Some(cert), Some(key)) => (cert.clone(), key.clone()),
        (None, None) => {
            if workers.iter().any(|w| w.signed_cert.is_some()) {
                anyhow::bail!(
                    "this dump carries worker certificates but no CA, so nothing could \
                     verify them; it was assembled wrongly"
                );
            }
            if files.contains_key(TOKENS_FILE) {
                anyhow::bail!("this dump carries {TOKENS_FILE} but no CA; secrets travel together");
            }
            return Ok(None);
        }
        (Some(_), None) => anyhow::bail!("{CA_KEY_FILE} is missing; half a CA is not a CA"),
        (None, Some(_)) => anyhow::bail!("{CA_CERT_FILE} is missing; half a CA is not a CA"),
    };

    let tokens: Vec<DumpToken> = match files.get(TOKENS_FILE) {
        Some(content) => serde_json::from_str(content)
            .map_err(|e| anyhow::anyhow!("{TOKENS_FILE} is not valid: {e}"))?,
        None => Vec::new(),
    };

    Ok(Some(DumpSecrets {
        ca_cert_pem,
        ca_key_pem,
        tokens,
    }))
}

fn parse<T: serde::de::DeserializeOwned>(
    files: &BTreeMap<String, String>,
    name: &str,
) -> anyhow::Result<T> {
    let content = files
        .get(name)
        .ok_or_else(|| anyhow::anyhow!("{name} is missing; this is not an AURCache dump"))?;
    serde_json::from_str(content).map_err(|e| anyhow::anyhow!("{name} is not valid: {e}"))
}

/// What a dump would do to one package, without doing it.
///
/// A dry run answers the only question worth asking before an import: which of
/// my packages does this touch. It reads, so it can be wrong about a race, but
/// nothing it reports has happened.
pub async fn preview(
    db: &DatabaseConnection,
    dump: &LoadedDump,
    options: &RestoreOptions,
) -> anyhow::Result<Vec<RestoreEntry>> {
    let mut entries = Vec::new();
    for pkgbase in dump.packages.keys() {
        let existing = package_row(db, pkgbase).await?;
        let dump_patch = dump.patches.get(pkgbase);
        entries.push(RestoreEntry {
            pkgbase: pkgbase.clone(),
            outcome: match (existing, options.on_existing) {
                (None, _) => RestoreOutcome::Imported,
                (Some(_), ExistingPackagePolicy::Skip) => RestoreOutcome::Skipped,
                (Some(_), ExistingPackagePolicy::Overwrite) => RestoreOutcome::Overwritten,
                (Some(row), ExistingPackagePolicy::MergePatches) => {
                    match patch_decision(row.patch.as_deref(), dump_patch.map(String::as_str)) {
                        PatchDecision::Keep => RestoreOutcome::Skipped,
                        PatchDecision::Adopt => RestoreOutcome::PatchAdopted,
                        PatchDecision::Conflict => RestoreOutcome::Failed {
                            error: PATCH_CONFLICT.to_string(),
                        },
                    }
                }
            },
        });
    }
    Ok(entries)
}

/// Said the same way wherever it is reported, so a dry run and a refused import
/// give the operator the same sentence.
const PATCH_CONFLICT: &str = "both this instance and the dump carry a patch for this package; \
     keep one and remove the other, or import with --on-existing overwrite";

/// What `merge-patches` should do about one package's patches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PatchDecision {
    /// Nothing to do: the instance's patch stands, or neither side has one.
    Keep,
    /// The instance has none and the dump does.
    Adopt,
    /// Both do. Not resolvable without reading them.
    Conflict,
}

fn patch_decision(existing: Option<&str>, from_dump: Option<&str>) -> PatchDecision {
    // An empty patch is the absence of one, not a patch that changes nothing.
    let has = |patch: Option<&str>| patch.is_some_and(|p| !p.trim().is_empty());
    match (has(existing), has(from_dump)) {
        (true, true) => PatchDecision::Conflict,
        (false, true) => PatchDecision::Adopt,
        _ => PatchDecision::Keep,
    }
}

async fn package_row(
    db: &DatabaseConnection,
    pkgbase: &str,
) -> anyhow::Result<Option<packages::Model>> {
    Ok(Packages::find()
        .filter(packages::Column::Name.eq(pkgbase))
        .one(db)
        .await?)
}

/// Apply a dump.
///
/// Three passes, and the order between the last two is the point -- see this
/// module's header. Rows first, with no network; then every package's source,
/// which is what fills in the names a dependency can be resolved by; only then
/// the dependency graph.
///
/// # What happens when part of it fails
///
/// The two halves fail differently, on purpose.
///
/// **Pass 1 is all or nothing.** It is one transaction over local rows, so
/// either every package the dump describes is written or none is. A dump that
/// half-applied would leave an instance that is neither what it was nor what
/// the dump says, which is worse than either.
///
/// **Passes 2 and 3 are per package, and a failure does not roll anything
/// back.** They read sources over the network and parse PKGBUILDs, so they fail
/// for reasons that have nothing to do with the rest of the dump: one
/// unparseable PKGBUILD, one unreachable remote, one dependency naming
/// something that does not exist. Undoing 199 good packages because the 200th
/// has a typo in it would be the wrong trade, and re-running the restore is
/// then the only way back.
///
/// The row is deliberately kept. It carries the platforms, flags and patch the
/// operator asked for, and the commonest reason to get here is a transient read
/// -- throwing that configuration away on a network blip would lose more than
/// it saves. What such a package does *not* have is its `provides` and split
/// package names, so a dependency naming one of those will not find it; the
/// entry says so, because it is the difference between a restore that worked
/// and one that looks like it did.
pub async fn apply(
    services: &Services,
    ca_dir: &std::path::Path,
    dump: LoadedDump,
    options: RestoreOptions,
    progress: Sender<RestoreEntry>,
) {
    let Services {
        store, db, repo, ..
    } = services;
    // PASS 1: rows. One transaction, because a half-applied dump is neither
    // what the instance was nor what the dump describes.
    let applied = match write_rows(db, repo, &dump, &options).await {
        Ok(applied) => applied,
        Err(e) => {
            // The transaction rolled back, so nothing was written; say so
            // against the dump as a whole rather than blaming one package.
            let _ = progress
                .send(RestoreEntry {
                    pkgbase: String::new(),
                    outcome: RestoreOutcome::Failed {
                        error: format!("nothing was imported: {e:#}"),
                    },
                })
                .await;
            return;
        }
    };

    for entry in &applied.entries {
        let _ = progress.send(entry.clone()).await;
    }

    // Written after the rows commit, and only then: replacing the CA on disk is
    // not something a rolled-back transaction can take back, so it must not
    // happen while the import might still fail.
    match write_secrets(&dump, &options, ca_dir) {
        Ok(true) => services.activity.emit(Event::RestoreCaReplaced {}),
        Ok(false) => {}
        Err(e) => {
            let _ = progress
                .send(RestoreEntry {
                    pkgbase: String::new(),
                    outcome: RestoreOutcome::Failed {
                        error: format!(
                            "the packages were imported, but the secrets were not: {e:#}"
                        ),
                    },
                })
                .await;
        }
    }

    // PASS 2: sources. Every imported package's `provides` and split package
    // names, so that pass 3 can recognise them. Until this is done for *all* of
    // them, resolving any one would fall through to the AUR and adopt a package
    // in place of one the dump carried.
    // A set: pass 3 tests every touched package against it, which is quadratic
    // over a `Vec`.
    let mut unresolved = HashSet::new();
    for pkgbase in &applied.touched {
        if let Err(e) = fill_source_facts(db, store, pkgbase).await {
            services.activity.emit(Event::RestorePackageFailed {
                pkg: pkgbase.as_str().into(),
                step: RestoreStep::Source,
                error: format!("{e:#}"),
            });
            let _ = progress
                .send(RestoreEntry {
                    pkgbase: pkgbase.clone(),
                    outcome: RestoreOutcome::Failed {
                        error: format!(
                            "imported, but its source could not be read, so other packages \
                         will not find it by its split package names or what it provides: {e:#}. \
                         Fix the source and restore again with --on-existing overwrite."
                        ),
                    },
                })
                .await;
            unresolved.insert(pkgbase.clone());
        }
    }

    // PASS 3: the graph, and the builds it makes possible.
    let mut restored: Vec<i32> = Vec::new();
    for pkgbase in &applied.touched {
        // Its source could not be read, so its dependency list cannot be
        // either. Trying anyway would fail identically and report it twice.
        if unresolved.contains(pkgbase) {
            continue;
        }
        let Ok(Some(row)) = package_row(db, pkgbase).await else {
            continue;
        };
        restored.push(row.id);
        // Edges only: removing orphans now would take a dependency-only
        // package whose dependent this pass has not reached yet -- nothing
        // points at it until then. They are collected once, below.
        if let Err(e) = crate::package::update::resync_graph(services, &row).await {
            services.activity.emit(Event::RestorePackageFailed {
                pkg: pkgbase.as_str().into(),
                step: RestoreStep::Dependencies,
                error: format!("{e:#}"),
            });
            let _ = progress
                .send(RestoreEntry {
                    pkgbase: pkgbase.clone(),
                    outcome: RestoreOutcome::Failed {
                        error: format!(
                            "imported, but its dependencies could not be resolved, so it will not \
                         build until they are: {e:#}"
                        ),
                    },
                })
                .await;
        }
    }

    // What the dump carried and nothing now needs, once every edge is known.
    // Only what this restore wrote is a candidate: a package some concurrent
    // add has inserted but not linked yet is not the restore's to judge.
    if let Err(e) =
        crate::package::live_check::live_check(db, &services.store, &services.repo, &restored, &[])
            .await
    {
        tracing::warn!("restore could not remove packages nothing needs: {e:#}");
    }

    // Builds for everything that has none: what the dump carried, and the
    // dependencies pass 3 brought in, each queued or left waiting on what it
    // needs. The same scan a server runs when it starts, so a restored
    // instance builds the same either way, without waiting for a restart.
    if let Err(e) =
        crate::package::enqueue::enqueue_missing_buildable_packages(db, &services.activity).await
    {
        let _ = progress
            .send(RestoreEntry {
                pkgbase: String::new(),
                outcome: RestoreOutcome::Failed {
                    error: format!(
                        "the packages were imported, but their builds were not queued: {e:#}. \
                         Restarting the server queues them."
                    ),
                },
            })
            .await;
    }
}

/// Install the dump's CA and token hashes, when asked to.
///
/// Replacing the CA invalidates every certificate the current workers hold, so
/// it happens only on request. `--clear` counts as that request: it already
/// says the dump replaces what it carries, and a CA is one of the things it
/// carries.
///
/// The CA is written to disk rather than the database, which is why it cannot
/// share the transaction that wrote the rows -- and why it is written after
/// them, once nothing is left that could still roll back.
///
/// Returns whether the CA was replaced.
fn write_secrets(
    dump: &LoadedDump,
    options: &RestoreOptions,
    ca_dir: &std::path::Path,
) -> anyhow::Result<bool> {
    let Some(secrets) = &dump.secrets else {
        return Ok(false);
    };
    if !take_secrets(options) {
        return Ok(false);
    }

    aurcache_ca::Ca::restore(
        ca_dir,
        secrets.ca_cert_pem.clone(),
        secrets.ca_key_pem.clone(),
    )?;
    Ok(true)
}

/// What pass 1 did.
#[derive(Debug)]
pub struct Applied {
    pub entries: Vec<RestoreEntry>,
    /// Packages whose sources and graph the later passes must visit. A skipped
    /// package is not among them: it was already here, with its own resolution
    /// already done.
    pub touched: Vec<String>,
}

/// Insert and update the rows, in one transaction.
///
/// Public because it is the whole of the import that touches no network: the
/// passes after it read sources and resolve dependencies, so this is the part
/// whose effect on the database can be stated exactly and tested directly.
pub async fn write_rows(
    db: &DatabaseConnection,
    repo: &Repository,
    dump: &LoadedDump,
    options: &RestoreOptions,
) -> anyhow::Result<Applied> {
    // Every conflict, before anything is written. Reporting them one at a time
    // would make an operator fix one, re-run, and meet the next; and a refusal
    // that has already applied half the dump is the state this whole design
    // exists to avoid.
    if options.on_existing == ExistingPackagePolicy::MergePatches {
        let mut conflicts = Vec::new();
        for pkgbase in dump.packages.keys() {
            let existing = package_row(db, pkgbase).await?;
            let decision = patch_decision(
                existing.as_ref().and_then(|row| row.patch.as_deref()),
                dump.patches.get(pkgbase).map(String::as_str),
            );
            if decision == PatchDecision::Conflict {
                conflicts.push(pkgbase.clone());
            }
        }
        if !conflicts.is_empty() {
            anyhow::bail!("{PATCH_CONFLICT}: {}", conflicts.join(", "));
        }
    }

    // A clear retires every published artifact with its rows, and only if the
    // transaction below commits; the sweep deletes the files later.
    let mut update = repo.begin(db).await?;
    if options.clear {
        for file in update.all_published_files(db).await? {
            update.retire(&file)?;
        }
    }
    let applied = update
        .commit(db, |txn| {
            let dump = dump.clone();
            let options = options.clone();
            Box::pin(async move { write_rows_txn(txn, &dump, &options).await })
        })
        .await?;

    // `--clear` removes every package, so every build log goes with them.
    if options.clear {
        crate::build_logger::remove_all_logs().await;
    }
    Ok(applied)
}

/// The transaction [`write_rows`] commits: run again whole if it fails.
async fn write_rows_txn(
    txn: &sea_orm::DatabaseTransaction,
    dump: &LoadedDump,
    options: &RestoreOptions,
) -> anyhow::Result<Applied> {
    let mut entries = Vec::new();
    let mut touched = Vec::new();
    let mut ids = BTreeMap::new();

    // Clearing happens inside the same transaction that writes the
    // replacement. If it did not, an import that failed after wiping would
    // leave an empty instance -- the one outcome worse than either keeping the
    // old state or taking the new one.
    if options.clear {
        clear_existing(txn).await?;
    }

    for (pkgbase, package) in &dump.packages {
        let existing = Packages::find()
            .filter(packages::Column::Name.eq(pkgbase))
            .one(txn)
            .await?;
        let patch = dump.patches.get(pkgbase).cloned();

        let outcome = match (existing, options.on_existing) {
            (Some(row), ExistingPackagePolicy::Skip) => {
                ids.insert(pkgbase.clone(), row.id);
                RestoreOutcome::Skipped
            }
            (Some(row), ExistingPackagePolicy::Overwrite) => {
                let id = row.id;
                let mut active: packages::ActiveModel = row.into();
                apply_config(&mut active, package, patch);
                active.update(txn).await?;
                ids.insert(pkgbase.clone(), id);
                touched.push(pkgbase.clone());
                RestoreOutcome::Overwritten
            }
            (Some(row), ExistingPackagePolicy::MergePatches) => {
                let id = row.id;
                // Conflicts were ruled out above, so this is only ever "the
                // instance has none and the dump does".
                let decision = patch_decision(row.patch.as_deref(), patch.as_deref());
                ids.insert(pkgbase.clone(), id);
                if decision == PatchDecision::Adopt {
                    let mut active: packages::ActiveModel = row.into();
                    active.patch = Set(patch);
                    active.update(txn).await?;
                    // A patch changes what the source resolves to, so its
                    // `provides`, split names and dependencies all have to be
                    // read again.
                    touched.push(pkgbase.clone());
                    RestoreOutcome::PatchAdopted
                } else {
                    RestoreOutcome::Skipped
                }
            }
            (None, _) => {
                let id = insert_row(txn, pkgbase, package, patch).await?;
                ids.insert(pkgbase.clone(), id);
                touched.push(pkgbase.clone());
                RestoreOutcome::Imported
            }
        };
        entries.push(RestoreEntry {
            pkgbase: pkgbase.clone(),
            outcome,
        });
    }

    write_settings(txn, dump, &ids).await?;
    write_workers(txn, dump, options).await?;
    write_tokens(txn, dump, options).await?;
    Ok(Applied { entries, touched })
}

/// Remove the rows of everything a dump replaces. Their artifacts leave the
/// repository through the update [`write_rows`] commits this in.
///
/// What gets cleared is decided by what a dump *contains*, not by a list of
/// tables: packages, settings and workers travel in every dump, so all three
/// go. Anything a dump does not carry is left alone -- which is what stops a
/// public dump, which has no CA, from destroying one.
///
/// Builds, files and VCS-source rows go with their packages. They are not in
/// the dump because they are derived, but leaving them would orphan them
/// against packages that no longer exist.
async fn clear_existing<C: sea_orm::ConnectionTrait>(txn: &C) -> anyhow::Result<()> {
    aurcache_db::prelude::Dependencies::delete_many()
        .exec(txn)
        .await?;
    aurcache_db::prelude::PackageVcsSources::delete_many()
        .exec(txn)
        .await?;
    aurcache_db::prelude::Files::delete_many().exec(txn).await?;
    aurcache_db::prelude::Builds::delete_many()
        .exec(txn)
        .await?;
    aurcache_db::prelude::Settings::delete_many()
        .exec(txn)
        .await?;
    aurcache_db::prelude::Workers::delete_many()
        .exec(txn)
        .await?;
    Packages::delete_many().exec(txn).await?;
    Ok(())
}

/// Restore the workers a dump trusts.
///
/// Identified by fingerprint, not by name: the fingerprint is what a worker
/// proves on contact, and two instances can easily have used different names
/// for the same machine.
///
/// No certificate is imported -- a public dump has none, and one signed by
/// another instance's CA would mean nothing here. A worker whose fingerprint is
/// already known re-enrolls and is approved without anyone being asked, which is
/// what makes a migration invisible to it.
async fn write_workers<C: sea_orm::ConnectionTrait>(
    txn: &C,
    dump: &LoadedDump,
    options: &RestoreOptions,
) -> anyhow::Result<()> {
    // A certificate is only worth restoring alongside the CA that signed it.
    let with_certificates = dump.secrets.is_some() && take_secrets(options);
    for worker in &dump.workers {
        let existing = aurcache_db::prelude::Workers::find()
            .filter(aurcache_db::workers::Column::CertFingerprint.eq(&worker.cert_fingerprint))
            .one(txn)
            .await?;
        // Already trusted here. Its routing and its settings are this
        // instance's decision, not the dump's, so it is left as it is.
        if existing.is_some() {
            continue;
        }
        let restored = aurcache_db::workers::ActiveModel {
            name: Set(worker.name.clone()),
            status: Set(aurcache_common::api::worker::ApprovalStatus::Approved),
            cert_fingerprint: Set(worker.cert_fingerprint.clone()),
            native_arches: Set(WorkerList::new(&worker.native_arches)),
            emulated_arches: Set(WorkerList::new(&worker.emulated_arches)),
            package_affinity: Set(WorkerList::new(&worker.package_affinity)),
            priority: Set(worker.priority),
            concurrency: Set(worker.concurrency),
            signed_cert: Set(with_certificates
                .then(|| worker.signed_cert.clone())
                .flatten()),
            not_after: Set(with_certificates.then_some(worker.not_after).flatten()),
            // Declared by the worker itself, when it next registers.
            settings_declaration: Set("[]".to_string()),
            ..Default::default()
        }
        .insert(txn)
        .await?;
        // Keyed to the row just made, so values follow the fingerprint rather
        // than whatever id the worker had where the dump was taken. Written
        // unchecked: there is no declaration to check them against until the
        // worker registers, and it refuses what it does not accept on delivery.
        for (key, value) in &worker.settings {
            aurcache_db::helpers::worker_store::upsert_worker_setting(txn, restored.id, key, value)
                .await?;
        }
    }
    Ok(())
}

/// Whether this import is taking the dump's secrets.
fn take_secrets(options: &RestoreOptions) -> bool {
    options.secrets == SecretsPolicy::Copy || options.clear
}

/// Restore the API token hashes, so tokens users already hold keep working.
///
/// Matched on the hash: the same token restored twice is the same token, and a
/// user who has since generated a new one here keeps it alongside rather than
/// having it replaced by an older one from the dump.
async fn write_tokens<C: sea_orm::ConnectionTrait>(
    txn: &C,
    dump: &LoadedDump,
    options: &RestoreOptions,
) -> anyhow::Result<()> {
    let Some(secrets) = &dump.secrets else {
        return Ok(());
    };
    if !take_secrets(options) {
        return Ok(());
    }

    for token in &secrets.tokens {
        let existing = aurcache_db::prelude::ApiTokens::find()
            .filter(aurcache_db::api_tokens::Column::TokenHash.eq(&token.token_hash))
            .one(txn)
            .await?;
        if existing.is_some() {
            continue;
        }
        aurcache_db::api_tokens::ActiveModel {
            username: Set(token.username.clone()),
            token_hash: Set(token.token_hash.clone()),
            ..Default::default()
        }
        .insert(txn)
        .await?;
    }
    Ok(())
}

/// The platforms a dump names for a package; [`load_dump`] has refused one
/// naming a platform this version does not know.
fn platforms(package: &DumpPackage) -> Platforms {
    Platforms::new(
        package
            .platforms
            .iter()
            .filter_map(|name| name.parse().ok()),
    )
}

/// The columns a dump owns. Everything else on the row is derived and is left
/// to the passes that follow.
fn apply_config(active: &mut packages::ActiveModel, package: &DumpPackage, patch: Option<String>) {
    active.platforms = Set(platforms(package));
    active.build_flags = Set(BuildFlags::new(&package.build_flags));
    active.directly_requested = Set(package.directly_requested);
    active.source_data = Set(package.source_data.clone());
    active.patch = Set(patch);
}

async fn insert_row<C: sea_orm::ConnectionTrait>(
    db: &C,
    pkgbase: &str,
    package: &DumpPackage,
    patch: Option<String>,
) -> anyhow::Result<i32> {
    Ok(packages::ActiveModel {
        name: Set(pkgbase.to_string()),
        // Nothing is built for it here yet, which is what the add path
        // records for a new package too.
        status: Set(BuildState::Enqueued),
        out_of_date: Set(false),
        platforms: Set(platforms(package)),
        build_flags: Set(BuildFlags::new(&package.build_flags)),
        source_data: Set(package.source_data.clone()),
        directly_requested: Set(package.directly_requested),
        patch: Set(patch),
        ..Default::default()
    }
    .insert(db)
    .await?
    .id)
}

/// Settings, keyed back from pkgbase onto row ids.
///
/// One read for every row the restore touches, then one batched insert for
/// the missing keys and one write per changed value: the per-key
/// select-then-write this replaces needed two round trips per setting
/// (~50 for a full dump), and rewrote rows whose value already matched.
async fn write_settings<C: sea_orm::ConnectionTrait>(
    db: &C,
    dump: &LoadedDump,
    ids: &BTreeMap<String, i32>,
) -> anyhow::Result<()> {
    /// One value the dump sets, where it applies.
    struct Desired<'a> {
        /// The stored `pkg_id`; see [`settings::scope`].
        scope: i32,
        key: &'a str,
        value: &'a str,
    }
    let mut desired: Vec<Desired> = Vec::new();
    for (key, value) in &dump.settings.global {
        desired.push(Desired {
            scope: settings::scope(None),
            key,
            value,
        });
    }
    for (pkgbase, values) in &dump.settings.packages {
        // Validation guarantees the package is in the dump; this only skips one
        // that was left alone by `Skip`, whose own settings are already right.
        let Some(id) = ids.get(pkgbase) else { continue };
        for (key, value) in values {
            desired.push(Desired {
                scope: settings::scope(Some(*id)),
                key,
                value,
            });
        }
    }
    // A dump from before a setting was retired still carries it; restoring it
    // would bring back a row the migration removed and nothing reads.
    desired.retain(|d| !aurcache_common::settings::RETIRED_SETTING_KEYS.contains(&d.key));
    // Nor a package value for a setting packages cannot override: nothing
    // would read it.
    desired.retain(|d| {
        d.scope == settings::scope(None)
            || Setting::from_key(d.key).is_none_or(|s| s.meta().scope == Scope::Package)
    });

    let scopes: HashSet<i32> = desired.iter().map(|d| d.scope).collect();
    let mut existing: BTreeMap<(i32, String), settings::Model> = settings::Entity::find()
        .filter(settings::Column::PkgId.is_in(scopes))
        .all(db)
        .await?
        .into_iter()
        .map(|row| ((row.pkg_id, row.key.clone()), row))
        .collect();

    let mut missing = Vec::new();
    for Desired { scope, key, value } in desired {
        match existing.remove(&(scope, key.to_string())) {
            // The settings table carries no timestamps, so a row that already
            // holds the restored value is done: rewriting it changes nothing.
            Some(row) if row.value.as_deref() == Some(value) => {}
            Some(row) => {
                let mut active: settings::ActiveModel = row.into();
                active.value = Set(Some(value.to_string()));
                active.update(db).await?;
            }
            None => missing.push(settings::ActiveModel {
                key: Set(key.to_string()),
                value: Set(Some(value.to_string())),
                pkg_id: Set(scope),
                ..Default::default()
            }),
        }
    }
    if !missing.is_empty() {
        settings::Entity::insert_many(missing).exec(db).await?;
    }
    Ok(())
}

/// Read a package's source and record what a dependency can find it by.
///
/// This is the step that makes the import self-contained. Without it the row
/// knows only its pkgbase, and a dependency naming one of its split packages or
/// something it provides would not match it.
async fn fill_source_facts(
    db: &DatabaseConnection,
    store: &SnapshotStore,
    pkgbase: &str,
) -> anyhow::Result<()> {
    let Some(row) = package_row(db, pkgbase).await? else {
        return Ok(());
    };
    let sourceinfo = store
        .sourceinfo(&row.source_data, row.patch.as_deref())
        .await?;
    let deps =
        aurcache_deps::deps_from_srcinfo(&sourceinfo, &crate::pkg::architectures(&row.platforms));

    let mut active: packages::ActiveModel = row.into();
    active.split_packages = Set(split_packages_json(pkgbase, &deps.pkgnames)?);
    active.provides = Set(provides_json(&deps.provides)?);
    active.upstream_version = Set(Some(sourceinfo.base.version.to_string()));
    active.update(db).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{clear_existing, load_dump};
    use aurcache_common::api::dump::DUMP_SCHEMA_VERSION;
    use aurcache_common::build_state::BuildState;
    use std::collections::BTreeMap;

    /// Build an archive from explicit file contents, so a test can write a
    /// malformed one as easily as a good one.
    fn archive(files: &[(&str, &str)]) -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut tar = tar::Builder::new(flate2::write::GzEncoder::new(
                &mut bytes,
                flate2::Compression::default(),
            ));
            for (path, content) in files {
                let mut header = tar::Header::new_gnu();
                header.set_path(path).unwrap();
                header.set_size(content.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                tar.append(&header, content.as_bytes()).unwrap();
            }
            tar.finish().unwrap();
        }
        bytes
    }

    fn manifest(version: u32) -> String {
        format!(
            r#"{{"schema_version":{version},"aurcache_version":"t","created_at":0,"includes_secrets":false}}"#
        )
    }

    const ONE_PACKAGE: &str = r#"{"hello":{"source_data":{"type":"aur","name":"hello"},
        "platforms":["x86_64"],"build_flags":[],"directly_requested":true}}"#;

    /// A minimal dump's files, by path.
    fn good() -> BTreeMap<&'static str, String> {
        BTreeMap::from([
            ("manifest.json", manifest(DUMP_SCHEMA_VERSION)),
            ("packages.json", ONE_PACKAGE.to_string()),
            (
                "settings.json",
                r#"{"global":{},"packages":{}}"#.to_string(),
            ),
            ("workers.json", "[]".to_string()),
        ])
    }

    fn load(files: BTreeMap<&str, String>) -> anyhow::Result<super::LoadedDump> {
        let owned: Vec<(&str, &str)> = files.iter().map(|(a, b)| (*a, b.as_str())).collect();
        load_dump(&archive(&owned))
    }

    #[test]
    fn a_well_formed_dump_loads() {
        let loaded = load(good()).unwrap();
        assert!(loaded.packages.contains_key("hello"));
    }

    /// The manifest's version is the load-bearing part of the format: restoring
    /// into a *different* AURCache is the point, so one that cannot understand
    /// the file must say so rather than apply the half it recognises.
    #[test]
    fn a_newer_dump_is_refused() {
        let mut files = good();
        files.insert("manifest.json", manifest(DUMP_SCHEMA_VERSION + 1));
        let error = load(files).unwrap_err().to_string();
        assert!(error.contains("upgrade AURCache"), "unhelpful: {error}");
    }

    #[test]
    fn something_that_is_not_a_dump_is_refused() {
        let error = load_dump(&archive(&[("hello.txt", "hi")]))
            .unwrap_err()
            .to_string();
        assert!(error.contains("not an AURCache dump"), "unhelpful: {error}");
    }

    /// A patch is attached by filename, so one naming a package that is not
    /// there was assembled wrongly. Dropping it silently would lose an edit
    /// somebody made on purpose.
    #[test]
    fn a_patch_without_its_package_is_refused() {
        let mut files = good();
        files.insert("patches/ghost.patch", "--- a\n".to_string());
        let error = load(files).unwrap_err().to_string();
        assert!(error.contains("no matching package"), "unhelpful: {error}");
    }

    /// Same reasoning for a setting: it would have nowhere to go.
    #[test]
    fn a_setting_for_an_absent_package_is_refused() {
        let mut files = good();
        files.insert(
            "settings.json",
            r#"{"global":{},"packages":{"ghost":{"k":"v"}}}"#.to_string(),
        );
        let error = load(files).unwrap_err().to_string();
        assert!(error.contains("not in packages.json"), "unhelpful: {error}");
    }

    /// A package with no platforms could never build, so importing it would
    /// produce a row that sits there doing nothing.
    #[test]
    fn a_package_with_no_platforms_is_refused() {
        let mut files = good();
        files.insert(
            "packages.json",
            r#"{"hello":{"source_data":{"type":"aur","name":"hello"},
                "platforms":[],"build_flags":[],"directly_requested":true}}"#
                .to_string(),
        );
        let error = load(files).unwrap_err().to_string();
        assert!(error.contains("no platforms"), "unhelpful: {error}");
    }

    /// Clearing is undone by a rollback.
    ///
    /// This is what lets `write_rows` clear and write in one transaction, and
    /// it is the property the whole `--clear` design rests on: if the write
    /// fails, the instance must still be what it was. An emptied instance is
    /// the one outcome worse than either keeping the old state or taking the
    /// new one, and it is not recoverable by re-running.
    #[tokio::test]
    async fn a_rolled_back_clear_puts_everything_back() {
        use aurcache_db::migration::Migrator;
        use aurcache_db::packages::SourceData;
        use sea_orm::{ActiveModelTrait, Database, EntityTrait, Set, TransactionTrait};
        use sea_orm_migration::MigratorTrait;

        let db = Database::connect("sqlite::memory:").await.unwrap();
        Migrator::up(&db, None).await.unwrap();
        aurcache_db::packages::ActiveModel {
            name: Set("precious".to_string()),
            status: Set(BuildState::Active),
            out_of_date: Set(false),
            build_flags: Set(Default::default()),
            platforms: Set("x86_64".parse().unwrap()),
            source_data: Set(SourceData::Aur {
                name: "precious".to_string(),
            }),
            directly_requested: Set(true),
            ..Default::default()
        }
        .insert(&db)
        .await
        .unwrap();

        let txn = db.begin().await.unwrap();
        clear_existing(&txn).await.unwrap();
        assert!(
            aurcache_db::prelude::Packages::find()
                .all(&txn)
                .await
                .unwrap()
                .is_empty(),
            "the clear did not take effect inside its transaction"
        );
        txn.rollback().await.unwrap();

        let survivors = aurcache_db::prelude::Packages::find()
            .all(&db)
            .await
            .unwrap();
        assert_eq!(
            survivors.len(),
            1,
            "a rolled-back clear destroyed data anyway"
        );
        assert_eq!(survivors[0].name, "precious");
    }

    /// A certificate is only meaningful under the CA that signed it, so a dump
    /// carrying certificates and no CA authenticates nobody -- while looking
    /// complete. Refused rather than partially applied.
    #[test]
    fn worker_certificates_without_a_ca_are_refused() {
        let mut files = good();
        files.insert(
            "workers.json",
            r#"[{"name":"w","cert_fingerprint":"fp","status":"approved","native_arches":["x86_64"],
                 "emulated_arches":[],"package_affinity":[],"priority":0,"concurrency":1,
                 "signed_cert":"-----BEGIN CERTIFICATE-----"}]"#
                .to_string(),
        );
        let error = load(files).unwrap_err().to_string();
        assert!(error.contains("no CA"), "unhelpful: {error}");
    }

    /// Half a CA is not a CA.
    #[test]
    fn a_ca_certificate_without_its_key_is_refused() {
        let mut files = good();
        files.insert("ca-cert.pem", "-----BEGIN CERTIFICATE-----".to_string());
        let error = load(files).unwrap_err().to_string();
        assert!(error.contains("half a CA"), "unhelpful: {error}");
    }

    /// The manifest's flag is how a reader knows what it is holding before it
    /// unpacks anything. One that disagrees with the contents means the archive
    /// was edited, and "the manifest says no secrets" is not a claim worth
    /// guessing at.
    #[test]
    fn a_manifest_that_lies_about_secrets_is_refused() {
        let mut files = good();
        files.insert("ca-cert.pem", "cert".to_string());
        files.insert("ca-key.pem", "key".to_string());
        // The manifest still says includes_secrets: false.
        let error = load(files).unwrap_err().to_string();
        assert!(error.contains("manifest says"), "unhelpful: {error}");
    }

    /// A well-formed private dump loads, and carries what it says.
    #[test]
    fn a_dump_with_secrets_loads() {
        let mut files = good();
        files.insert(
            "manifest.json",
            format!(
                r#"{{"schema_version":{DUMP_SCHEMA_VERSION},"aurcache_version":"t","created_at":0,"includes_secrets":true}}"#
            ),
        );
        files.insert("ca-cert.pem", "cert".to_string());
        files.insert("ca-key.pem", "key".to_string());
        files.insert(
            "tokens.json",
            r#"[{"username":"alice","token_hash":"abc"}]"#.to_string(),
        );

        let loaded = load(files).unwrap();
        let secrets = loaded.secrets.expect("secrets were dropped");
        assert_eq!(secrets.ca_key_pem, "key");
        assert_eq!(secrets.tokens.len(), 1);
        assert_eq!(secrets.tokens[0].username, "alice");
    }

    /// A patch reaches the package it names.
    #[test]
    fn a_patch_is_matched_to_its_package() {
        let mut files = good();
        files.insert("patches/hello.patch", "--- a\n+++ b\n".to_string());
        let loaded = load(files).unwrap();
        assert_eq!(
            loaded.patches.get("hello").map(String::as_str),
            Some("--- a\n+++ b\n")
        );
    }
}
