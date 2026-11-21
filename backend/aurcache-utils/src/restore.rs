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

use anyhow::Context;
use aurcache_db::lists::{BuildFlags, Platforms, WorkerList};
use std::collections::{BTreeMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use aurcache_common::api::dump::{
    CA_CERT_FILE, CA_KEY_FILE, DUMP_SCHEMA_VERSION, DumpManifest, DumpPackage, DumpPackages,
    DumpSecrets, DumpSettings, DumpToken, DumpWorker, ExistingPackagePolicy, MANIFEST_FILE,
    PACKAGES_FILE, PATCH_DIR, RestoreEntry, RestoreOptions, RestoreOutcome, SETTINGS_FILE,
    SIGNING_KEY_FILE, Secret, SecretChange, SecretReport, SecretsPolicy, TOKENS_FILE, WORKERS_FILE,
};
use aurcache_common::build_state::BuildState;
use aurcache_common::settings::{Scope, Setting};
use aurcache_db::packages;
use aurcache_db::prelude::Packages;
use flate2::read::GzDecoder;
use sea_orm::ActiveValue::Set;
use sea_orm::{ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};
use tokio::sync::mpsc::Sender;

use crate::package::source_facts::{SourceFacts, pkg_deps, record_source_names};
use crate::repository::{CommitOutcome, FinalizationPending, KeyReplacement, KeySwap, Repository};
use crate::services::Services;
use crate::settings::Change;
use crate::snapshot::SnapshotStore;
use aurcache_activitylog::activity_utils::ActivityLog;
use aurcache_activitylog::events::{Event, RestoreStep};
use aurcache_ca::StagedCa;
use aurcache_common::fs::{
    Installed, KeptCopy, Readers, Staged, check_replaceable, same_file, stage_private,
};
use aurcache_signing::{InstanceKey, KeyLocation};

/// A dump that has been read and checked, ready to apply. Its `Debug` prints
/// no secret, since that of [`DumpSecrets`] says only what it holds.
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
        aurcache_common::build_flags::canonical(&package.build_flags)
            .map_err(|e| anyhow::anyhow!("'{pkgbase}': {e}"))?;
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
            for alone in [TOKENS_FILE, SIGNING_KEY_FILE] {
                if files.contains_key(alone) {
                    anyhow::bail!("this dump carries {alone} but no CA; secrets travel together");
                }
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

    // Read, not unlocked: whether this server can unlock it is a question for
    // the restore that would install it, not for reading the dump.
    let signing_key = files.get(SIGNING_KEY_FILE).cloned();
    if let Some(key) = &signing_key {
        aurcache_signing::inspect(key.as_bytes())
            .map_err(|e| anyhow::anyhow!("{SIGNING_KEY_FILE} is not a signing key: {e:#}"))?;
    }

    Ok(Some(DumpSecrets {
        ca_cert_pem,
        ca_key_pem,
        tokens,
        signing_key,
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

/// What a dump would do to each package, without doing it.
///
/// A dry run answers the only question worth asking before an import: which of
/// my packages does this touch. What it would do to the server's secrets is
/// the plan's to say ([`plan_secrets`], [`SecretsPlan::expected`]). It reads,
/// so it can be wrong about a race, but nothing it reports has happened.
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
///
/// `plan` is what [`plan_secrets`] made of the dump's secrets before the
/// restore was accepted: refusing it is the caller's, since a restore that
/// could not finish is refused whole, before anything is written.
pub async fn apply(
    services: &Services,
    plan: SecretsPlan,
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
            report_failure(&progress, "", format!("nothing was imported: {e:#}")).await;
            return;
        }
    };

    match &applied.commit {
        RowsCommit::Done => {}
        RowsCommit::FilesPending { error } => {
            // The rows committed: the repository's files catch up through the
            // recorded plan, and the restore goes on.
            report_failure(
                &progress,
                "",
                format!(
                    "the packages were imported, but the repository's files are not all in \
                     place yet ({error}); the next repository update or restart puts them there"
                ),
            )
            .await;
        }
        RowsCommit::Unknown { error } => {
            // Neither "imported" nor "not imported" would be true, and the
            // secrets go with the rows: put in place over rows that did not
            // commit, they would leave the instance as neither the dump nor
            // what it was. The plan is dropped, and nothing changes them.
            drop(plan);
            report_failure(
                &progress,
                "",
                format!(
                    "whether the packages were imported is unknown: the database did not \
                     confirm the commit ({error}). The worker CA and the signing key were left \
                     as they are. Check which of the dump's packages are here, then restore it \
                     again, with --on-existing overwrite if any are"
                ),
            )
            .await;
            return;
        }
    }
    send_all(&progress, applied.entries.clone()).await;

    // Put in place after the rows commit, and only then: replacing the CA on
    // disk is not something a rolled-back transaction can take back, so it
    // must not happen while the import might still fail.
    let secrets = apply_secrets(plan, repo).await;
    record_secrets(&services.activity, &secrets);
    send_all(&progress, secret_failures(&secrets)).await;

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
            report_failure(
                &progress,
                pkgbase,
                format!(
                    "imported, but its source could not be read, so other packages \
                         will not find it by its split package names or what it provides: {e:#}. \
                         Fix the source and restore again with --on-existing overwrite."
                ),
            )
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
            report_failure(
                &progress,
                pkgbase,
                format!(
                    "imported, but its dependencies could not be resolved, so it will not \
                         build until they are: {e:#}"
                ),
            )
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
        report_failure(
            &progress,
            "",
            format!(
                "the packages were imported, but their builds were not queued: {e:#}. \
                         Restarting the server queues them."
            ),
        )
        .await;
    }
}

/// Report that restoring `pkgbase` failed -- the dump as a whole when empty.
///
/// A closed channel is ignored: nobody watching is a normal state, and the
/// restore carries on regardless.
async fn report_failure(progress: &Sender<RestoreEntry>, pkgbase: &str, error: String) {
    let _ = progress
        .send(RestoreEntry {
            pkgbase: pkgbase.to_string(),
            outcome: RestoreOutcome::Failed { error },
        })
        .await;
}

/// Report `entries`, in order; a closed channel is ignored, as by
/// [`report_failure`].
async fn send_all(progress: &Sender<RestoreEntry>, entries: Vec<RestoreEntry>) {
    for entry in entries {
        let _ = progress.send(entry).await;
    }
}

/// What restoring a dump's secrets will do, decided -- and everything it
/// writes written beside what it replaces -- before the rows are
/// ([`plan_secrets`]), and put in place once they have committed
/// ([`apply_secrets`]). Dropped instead, it removes what it wrote, and
/// nothing in place was touched.
pub struct SecretsPlan {
    /// What becomes of the worker CA.
    ca: CaPlan,
    /// What becomes of the repository's signing key.
    signing_key: KeyPlan,
    /// Where the server keeps its key, which the plan was made for.
    location: KeyLocation,
}

/// What a restore does to the worker CA.
enum CaPlan {
    /// Leave it as it is: the dump's secrets are not taken.
    Keep,
    /// The dump's is the one in place already -- the same certificate -- so
    /// there is nothing to write: what restoring the same dump again finds.
    Same,
    /// The dump's, checked as a pair and staged to replace the one in place.
    Replace(StagedCa),
}

/// What a restore does to the repository's signing key. Whatever the
/// server's own key file held before it is written or removed is kept
/// beside it ([`Previous`]).
enum KeyPlan {
    /// Leave it as it is: neither the dump's secrets taken nor `--clear`, or
    /// nothing to clear.
    Keep,
    /// Put the dump's key, which the server is to sign with, in its own key
    /// file, where it is staged: unlocked here, so it can sign, and locked
    /// with this server's passphrase if it came unlocked.
    Install {
        key: Box<InstanceKey>,
        staged: Staged,
        previous: Previous,
    },
    /// Put the dump's key, `restored`, in the server's own key file without
    /// signing with it -- the server signs with a key it is supplied, or not
    /// at all -- for when it does: checked against this server's passphrase,
    /// if it has one, and locked with it if it came unlocked.
    Store {
        restored: String,
        staged: Staged,
        previous: Previous,
    },
    /// `--clear` with no key in the dump, on a server that signs with its own
    /// key: a new one, staged to replace its key file, as a new instance
    /// generates its key at start.
    Generate {
        key: Box<InstanceKey>,
        staged: Staged,
        previous: Previous,
    },
    /// `--clear` with no key in the dump, on a server that does not sign
    /// with its own key: its own key file, at `path`, goes, as a new
    /// instance has none.
    Remove { path: PathBuf, previous: Previous },
    /// The server's own key file is the one `AURCACHE_SIGNING_KEY_FILE`
    /// names, which a restore never writes or removes: the dump's key, with
    /// this fingerprint, is not put anywhere, so nothing is asked of it.
    Supplied { restored: String },
}

/// What the server's own key file holds before a plan writes or removes it:
/// a key nothing else may have a copy of, so it is kept beside it rather than
/// lost.
struct Previous {
    /// What it will be reported as once kept.
    expected: PreviousKey,
    /// A copy staged to be kept as `<file>.replaced-<tag>`
    /// ([`aurcache_common::fs::stage_replaced`]), when there is anything to
    /// keep.
    copy: Option<KeptCopy>,
}

impl Previous {
    /// What the server's own key file `own` holds, for a plan that writes
    /// `written` there -- `None` for one that removes it, or writes a new
    /// key -- with what it holds staged to be kept: named for its key's
    /// fingerprint, or for its contents when it does not read as a key. Only
    /// the very bytes being written are not kept again: the same key may
    /// hold more than the one written over it -- another subkey, a
    /// certification -- and a copy already kept with the same bytes is not
    /// written twice ([`aurcache_common::fs::stage_replaced`]).
    fn of(own: &Path, written: Option<&[u8]>) -> anyhow::Result<Self> {
        let held = match std::fs::read(own) {
            Ok(held) => held,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self {
                    expected: PreviousKey::Absent,
                    copy: None,
                });
            }
            Err(error) => {
                return Err(anyhow::Error::new(error).context(format!(
                    "reading the signing key {} to keep it",
                    own.display()
                )));
            }
        };
        if written == Some(held.as_slice()) {
            return Ok(Self {
                expected: PreviousKey::Same,
                copy: None,
            });
        }
        let fingerprint = aurcache_signing::inspect(&held)
            .ok()
            .map(|facts| facts.fingerprint);
        let tag = fingerprint
            .clone()
            .unwrap_or_else(|| aurcache_common::digest::content_tag(&held));
        let copy = aurcache_common::fs::stage_replaced(own, &tag, &held, Readers::Owner)
            .with_context(|| {
            format!(
                "writing a copy of the signing key {} to keep",
                own.display()
            )
        })?;
        Ok(Self {
            expected: PreviousKey::Kept {
                fingerprint,
                kept: copy.path().to_path_buf(),
            },
            copy: Some(copy),
        })
    }

    /// Put the copy where it is kept: before the file is replaced or
    /// removed.
    fn keep(self) -> std::io::Result<Installed> {
        match self.copy {
            Some(copy) => copy.install(),
            None => Ok(Installed::Synced),
        }
    }
}

/// Where this server keeps its secrets, and how it uses its signing key:
/// what a restore's secrets are planned for, read from the environment and
/// the repository apart so a test can give it.
pub struct SecretsConfig {
    /// The worker CA's directory, where the server keeps its own key too.
    pub ca_dir: PathBuf,
    pub location: KeyLocation,
    /// The file holding what unlocks a locked key and locks one a restore
    /// writes (`AURCACHE_SIGNING_KEY_PASSPHRASE_FILE`). Read only by a plan
    /// that writes a key, so that a restore that writes none does not
    /// depend on it.
    pub passphrase_file: Option<PathBuf>,
    /// Whether the repository signs: `--clear` generates a key only for a
    /// server that signs with its own, and only then is the dump's key used.
    pub signs: bool,
}

impl SecretsConfig {
    /// As the environment configures it, for a server whose CA directory is
    /// `ca_dir` and whose repository is `repo`. Nothing is read but the
    /// environment.
    #[must_use]
    pub fn from_env(ca_dir: &Path, repo: &Repository) -> Self {
        Self {
            ca_dir: ca_dir.to_path_buf(),
            location: KeyLocation::from_env(ca_dir),
            passphrase_file: aurcache_signing::passphrase_file_from_env(),
            signs: repo.key().is_some(),
        }
    }
}

/// Work out what restoring `dump`'s secrets will do, and refuse it now if
/// any of it cannot be done: a CA that is not a pair, a key to sign with
/// that this server cannot unlock or that signs nothing -- it would stop the
/// server at its next start -- a key to keep that this server's passphrase
/// does not unlock, or a file it cannot write or replace, such as a CA
/// directory mounted read-only or a key file mounted in place. What it is to
/// write is written beside what it replaces, so that a full disk refuses the
/// restore too -- the copy of the key file it replaces or removes included.
///
/// Only what is written is checked, and as it will be used: the dump's key
/// is unlocked and must sign when the server is to sign with it, is checked
/// against this server's passphrase when it is only kept, and is not checked
/// at all when it is not written. A key that comes unlocked onto a server
/// that locks its keys is locked with its passphrase. The passphrase file is
/// read only when a key is written.
///
/// The secrets are taken with `--secrets copy` or `--clear`. `--clear` also
/// leaves the signing key as a new instance would have it: the dump's, or a
/// new one -- none, on a server that does not sign with its own. A key
/// `AURCACHE_SIGNING_KEY_FILE` supplies is the operator's, and is never
/// written or removed, even when it is the file where the server keeps its
/// own.
///
/// Unlocking a key and generating one are slow by design, so this runs on a
/// blocking thread.
pub async fn plan_secrets(
    dump: &LoadedDump,
    options: &RestoreOptions,
    config: SecretsConfig,
) -> anyhow::Result<SecretsPlan> {
    let secrets = dump.secrets.clone().filter(|_| take_secrets(options));
    let clear = options.clear;
    tokio::task::spawn_blocking(move || plan(secrets, clear, config)).await?
}

fn plan(
    secrets: Option<DumpSecrets>,
    clear: bool,
    config: SecretsConfig,
) -> anyhow::Result<SecretsPlan> {
    let SecretsConfig {
        ca_dir,
        location,
        passphrase_file,
        signs,
    } = config;
    let own = ca_dir.join(aurcache_signing::KEY_FILE);
    let own_is_supplied = matches!(&location, KeyLocation::Supplied(path) if same_file(path, &own));
    // Whether a key in the server's own file is the one it signs with.
    let own_signs = signs && matches!(location, KeyLocation::Own(_));
    let ca = match &secrets {
        None => CaPlan::Keep,
        Some(secrets) if aurcache_ca::Ca::holds(&ca_dir, &secrets.ca_cert_pem) => CaPlan::Same,
        Some(secrets) => CaPlan::Replace(
            aurcache_ca::Ca::stage(
                &ca_dir,
                secrets.ca_cert_pem.clone(),
                secrets.ca_key_pem.clone(),
            )
            .context("the dump's CA cannot be put in place")?,
        ),
    };
    // Read only for a key that is written: a restore that writes none goes
    // through whatever has become of the file.
    let passphrase = || {
        passphrase_file
            .as_deref()
            .map(aurcache_signing::read_passphrase)
            .transpose()
    };
    let carried = secrets
        .as_ref()
        .and_then(|secrets| secrets.signing_key.as_deref());
    let signing_key = match carried {
        Some(text) if own_is_supplied => KeyPlan::Supplied {
            restored: fingerprint(text)?,
        },
        Some(text) if own_signs => {
            replaceable_key(&own)?;
            let ToSign { text, key } = to_sign(text, passphrase()?.as_deref())?;
            KeyPlan::Install {
                previous: Previous::of(&own, Some(text.as_bytes()))?,
                staged: stage_key(&own, text.as_bytes())?,
                key: Box::new(key),
            }
        }
        Some(text) => {
            replaceable_key(&own)?;
            let restored = fingerprint(text)?;
            let text = to_keep(text, &restored, passphrase()?.as_deref())?;
            KeyPlan::Store {
                previous: Previous::of(&own, Some(text.as_bytes()))?,
                staged: stage_key(&own, text.as_bytes())?,
                restored,
            }
        }
        None if clear && !own_is_supplied && own_signs => {
            replaceable_key(&own)?;
            let key = InstanceKey::generate(passphrase()?.as_deref())?;
            KeyPlan::Generate {
                previous: Previous::of(&own, None)?,
                staged: key.stage(&own)?,
                key: Box::new(key),
            }
        }
        None if clear && !own_is_supplied && own.exists() => {
            replaceable_key(&own)?;
            KeyPlan::Remove {
                previous: Previous::of(&own, None)?,
                path: own,
            }
        }
        None => KeyPlan::Keep,
    };
    Ok(SecretsPlan {
        ca,
        signing_key,
        location,
    })
}

/// Refuse a restore that would have to replace or remove the server's own
/// key file `own` and cannot, saying what to do about it.
fn replaceable_key(own: &Path) -> anyhow::Result<()> {
    check_replaceable(own).with_context(|| {
        format!(
            "the server's signing key {} cannot be replaced; if it is a key you supply, \
             name it with AURCACHE_SIGNING_KEY_FILE, which a restore never writes",
            own.display()
        )
    })
}

/// Stage `text` beside the server's own key file `own`.
fn stage_key(own: &Path, text: &[u8]) -> anyhow::Result<Staged> {
    stage_private(own, text)
        .with_context(|| format!("writing beside the signing key {}", own.display()))
}

/// The dump's signing key `text`'s fingerprint, which reading the dump
/// already checked it has.
fn fingerprint(text: &str) -> anyhow::Result<String> {
    Ok(aurcache_signing::inspect(text.as_bytes())
        .context("the dump's signing key cannot be read")?
        .fingerprint)
}

/// The dump's key, as it will be written and as the server will sign with
/// it.
struct ToSign {
    text: String,
    key: InstanceKey,
}

/// The dump's signing key `text`, made ready to sign with on this server:
/// unlocked with its `passphrase`, saying what to configure when it cannot
/// be, and refused if it signs nothing now; locked with that passphrase
/// first if it came unlocked, as the server locks its own.
fn to_sign(text: &str, passphrase: Option<&str>) -> anyhow::Result<ToSign> {
    let facts = aurcache_signing::inspect(text.as_bytes())
        .context("the dump's signing key cannot be read")?;
    let text = match passphrase {
        Some(passphrase) if !facts.locked => aurcache_signing::locked(text.as_bytes(), passphrase)
            .context("locking the dump's signing key with AURCACHE_SIGNING_KEY_PASSPHRASE_FILE")?,
        _ => text.to_string(),
    };
    let key = InstanceKey::parse(text.as_bytes(), passphrase).map_err(|e| {
        if facts.locked && passphrase.is_none() {
            anyhow::anyhow!(
                "the dump's signing key {} is locked with a passphrase, and this server has \
                 none: set AURCACHE_SIGNING_KEY_PASSPHRASE_FILE to the passphrase it was \
                 locked with, restart, and restore again",
                facts.fingerprint
            )
        } else if facts.locked {
            e.context(format!(
                "the dump's signing key {} cannot be unlocked here; is \
                 AURCACHE_SIGNING_KEY_PASSPHRASE_FILE the passphrase it was locked with?",
                facts.fingerprint
            ))
        } else {
            e.context(format!(
                "the dump's signing key {} cannot sign here",
                facts.fingerprint
            ))
        }
    })?;
    Ok(ToSign { text, key })
}

/// The dump's signing key `text`, with fingerprint `restored`, made ready to
/// keep on this server without signing with it: whether it signs now is not
/// asked, but with a `passphrase` configured, it is the one that would
/// unlock it, so a locked key has to open with it and an unlocked one is
/// locked with it.
fn to_keep(text: &str, restored: &str, passphrase: Option<&str>) -> anyhow::Result<String> {
    let Some(passphrase) = passphrase else {
        return Ok(text.to_string());
    };
    let facts = aurcache_signing::inspect(text.as_bytes())
        .context("the dump's signing key cannot be read")?;
    if !facts.locked {
        return aurcache_signing::locked(text.as_bytes(), passphrase)
            .context("locking the dump's signing key with AURCACHE_SIGNING_KEY_PASSPHRASE_FILE");
    }
    aurcache_signing::check_passphrase(text.as_bytes(), passphrase).with_context(|| {
        format!(
            "the dump's signing key {restored} does not unlock with \
             AURCACHE_SIGNING_KEY_PASSPHRASE_FILE, which would unlock it here"
        )
    })?;
    Ok(text.to_string())
}

/// What putting a dump's secrets in place did, secret by secret: what a
/// restore reports, once the rows have committed, as exactly what changed --
/// or, from [`SecretsPlan::expected`], what a dry run says it would.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretsApplied {
    pub ca: CaOutcome,
    pub signing_key: KeyOutcome,
}

/// What a restore did to the worker CA.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaOutcome {
    /// Nothing: the dump's secrets were not taken.
    Kept,
    /// Nothing: the dump's CA is the one in place already.
    Same,
    /// The dump's CA is in place; the previous one is not kept. `unsynced`
    /// says why its directory could not be synced after, when it could not:
    /// a crash soon after may bring the previous one back.
    Replaced { unsynced: Option<String> },
    /// The dump's CA could not be put in place, and the previous one stays
    /// (at the latest from the next start, which repairs a pair left half
    /// replaced from its identity copy).
    Failed { error: String },
}

/// What a restore did to the repository's signing key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyOutcome {
    /// Nothing.
    Kept,
    /// Put the dump's key, with fingerprint `restored`, in the server's own
    /// key file; `signs_with` says what the server signs with now.
    Installed {
        restored: String,
        signs_with: SignsWith,
        file: KeyFile,
    },
    /// `--clear` without a key in the dump: the server's own key file is
    /// gone, or replaced by a new key, `generated`, which a server that
    /// signs with its own key signs with now; `supplied` is the key the
    /// server signs with otherwise, one `AURCACHE_SIGNING_KEY_FILE` supplies.
    Reset {
        generated: Option<String>,
        supplied: Option<String>,
        file: KeyFile,
    },
    /// The dump's key, `restored`, was not put anywhere: where the server
    /// keeps its own key is the file `AURCACHE_SIGNING_KEY_FILE` supplies,
    /// which a restore never writes; `supplied` is its key (`None` when it
    /// cannot be read), which the server signs with if `signs`.
    NotWritten {
        restored: String,
        supplied: Option<String>,
        signs: bool,
    },
    /// What the plan was to do could not be done, and the key file and the
    /// key the server signs with are as they were: `error` says what.
    Failed { error: String },
}

/// What a restore did to the server's own key file, beside what it put
/// there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyFile {
    pub path: PathBuf,
    /// What the file held before.
    pub previous: PreviousKey,
    /// Why its directory could not be synced after, when it could not: the
    /// change is in place, and every reader sees it, but a crash soon after
    /// may undo it.
    pub unsynced: Option<String>,
}

/// What the server's own key file held before a restore wrote or removed
/// it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreviousKey {
    /// Nothing: there was no file.
    Absent,
    /// The very bytes written in its place: nothing is kept.
    Same,
    /// Something else, kept beside it at `kept`, owner-only: a key with
    /// this `fingerprint`, or `None` for a file that does not read as one.
    Kept {
        fingerprint: Option<String>,
        kept: PathBuf,
    },
}

impl PreviousKey {
    /// The fingerprint of the key the file held, when it held one that
    /// reads.
    #[must_use]
    pub fn fingerprint(&self) -> Option<&str> {
        match self {
            Self::Kept { fingerprint, .. } => fingerprint.as_deref(),
            Self::Absent | Self::Same => None,
        }
    }

    /// Where it is kept, when it is.
    #[must_use]
    pub fn kept(&self) -> Option<&Path> {
        match self {
            Self::Kept { kept, .. } => Some(kept),
            Self::Absent | Self::Same => None,
        }
    }
}

/// What the server signs with once a restored key is in place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignsWith {
    /// The restored key, in place of the key with this fingerprint; the
    /// backfill signs again what that one signed.
    Restored { replaced: String },
    /// The restored key, which it already signed with -- the same primary
    /// key -- through another of its component keys than before: clients
    /// trust it already, but have to import it again to know the subkey that
    /// signs, and the backfill signs again what the one before signed.
    NewSigner,
    /// The restored key, which it already signed with.
    Same,
    /// The key `AURCACHE_SIGNING_KEY_FILE` supplies -- the operator's choice,
    /// which a restore does not override -- with this fingerprint (`None`
    /// when it cannot be read), which it signs with if `signs` and would
    /// sign with otherwise.
    Supplied {
        fingerprint: Option<String>,
        signs: bool,
    },
    /// Nothing: the server does not sign. The restored key is the one it
    /// signs with once `AURCACHE_SIGN` is on.
    Nothing,
}

impl SignsWith {
    /// What the server signs with once `replacement` swapped in the
    /// restored key.
    fn after(replacement: KeyReplacement) -> Self {
        match replacement {
            KeyReplacement::Replaced { previous } => Self::Restored { replaced: previous },
            KeyReplacement::SignerChanged => Self::NewSigner,
            KeyReplacement::Unchanged => Self::Same,
            KeyReplacement::NotSigning => Self::Nothing,
        }
    }

    /// What the server signs with once the restored key `restored` is in
    /// its own key file without being swapped in: what it signed with
    /// before.
    async fn beside(location: &KeyLocation, restored: &str, repo: &Repository) -> Self {
        match location {
            KeyLocation::Own(_) => Self::Nothing,
            KeyLocation::Supplied(_) => {
                let fingerprint = supplied_key(location, repo).await;
                if fingerprint.as_deref() == Some(restored) {
                    Self::Same
                } else {
                    Self::Supplied {
                        fingerprint,
                        signs: repo.key().is_some(),
                    }
                }
            }
        }
    }
}

impl SecretsPlan {
    /// What applying it would report should every rename succeed: what a
    /// dry run says becomes of the secrets.
    pub async fn expected(&self, repo: &Repository) -> SecretsApplied {
        let ca = match self.ca {
            CaPlan::Keep => CaOutcome::Kept,
            CaPlan::Same => CaOutcome::Same,
            CaPlan::Replace(_) => CaOutcome::Replaced { unsynced: None },
        };
        let file = |path: &Path, previous: &Previous| KeyFile {
            path: path.to_path_buf(),
            previous: previous.expected.clone(),
            unsynced: None,
        };
        let signing_key = match &self.signing_key {
            KeyPlan::Keep => KeyOutcome::Kept,
            KeyPlan::Supplied { restored } => KeyOutcome::NotWritten {
                restored: restored.clone(),
                supplied: supplied_key(&self.location, repo).await,
                signs: repo.key().is_some(),
            },
            KeyPlan::Install {
                key,
                staged,
                previous,
            } => KeyOutcome::Installed {
                restored: key.fingerprint().to_string(),
                signs_with: SignsWith::after(repo.replacing_with(key)),
                file: file(staged.target(), previous),
            },
            KeyPlan::Store {
                restored,
                staged,
                previous,
            } => KeyOutcome::Installed {
                restored: restored.clone(),
                signs_with: SignsWith::beside(&self.location, restored, repo).await,
                file: file(staged.target(), previous),
            },
            KeyPlan::Generate {
                key,
                staged,
                previous,
            } => KeyOutcome::Reset {
                generated: Some(key.fingerprint().to_string()),
                supplied: None,
                file: file(staged.target(), previous),
            },
            KeyPlan::Remove { path, previous } => KeyOutcome::Reset {
                generated: None,
                supplied: repo.key().map(|key| key.fingerprint().to_string()),
                file: file(path, previous),
            },
        };
        SecretsApplied { ca, signing_key }
    }
}

/// Put in place what `plan` staged, once the rows have committed.
///
/// The CA and the key are files, not rows, which is why they cannot share
/// the transaction that wrote the rows -- and why they are put in place
/// after it, once nothing is left that could roll back. Everything that
/// could refuse was checked, and everything new written, by
/// [`plan_secrets`]: what is left is renames, and the removal of a key file
/// `--clear` takes away, which can still fail, and each secret says whether
/// its own did. A rename that succeeded is in place even when its directory
/// could not be synced after, and is reported so.
pub async fn apply_secrets(plan: SecretsPlan, repo: &Repository) -> SecretsApplied {
    let SecretsPlan {
        ca,
        signing_key,
        location,
    } = plan;
    let ca = match ca {
        CaPlan::Keep => CaOutcome::Kept,
        CaPlan::Same => CaOutcome::Same,
        CaPlan::Replace(staged) => match blocking(move || staged.install()).await {
            Ok(installed) => CaOutcome::Replaced {
                unsynced: unsynced(installed),
            },
            Err(e) => CaOutcome::Failed {
                error: format!("{e:#}"),
            },
        },
    };
    let signing_key = apply_key(signing_key, location, repo).await;
    SecretsApplied { ca, signing_key }
}

/// Do what `plan` decided for the signing key. The key file and the key
/// the server signs with change together, under the repository lock
/// ([`Repository::replace_key`]), or not at all.
async fn apply_key(plan: KeyPlan, location: KeyLocation, repo: &Repository) -> KeyOutcome {
    let current = || {
        repo.key().map_or_else(
            || "the server does not sign".to_string(),
            |key| format!("the server signs with {} as before", key.fingerprint()),
        )
    };
    match plan {
        KeyPlan::Keep => KeyOutcome::Kept,
        KeyPlan::Supplied { restored } => {
            let supplied = supplied_key(&location, repo).await;
            KeyOutcome::NotWritten {
                restored,
                supplied,
                signs: repo.key().is_some(),
            }
        }
        KeyPlan::Install {
            key,
            staged,
            previous,
        } => {
            let restored = key.fingerprint().to_string();
            let target = staged.target().display().to_string();
            match repo
                .replace_key(*key, move || persist_key(previous, staged))
                .await
            {
                Ok(KeySwap {
                    replacement,
                    persisted,
                }) => KeyOutcome::Installed {
                    restored,
                    signs_with: SignsWith::after(replacement),
                    file: persisted,
                },
                Err(e) => KeyOutcome::Failed {
                    error: format!(
                        "the dump's signing key {restored} could not be put in {target} \
                         ({e:#}); {}",
                        current()
                    ),
                },
            }
        }
        KeyPlan::Store {
            restored,
            staged,
            previous,
        } => {
            let target = staged.target().display().to_string();
            match blocking(move || persist_key(previous, staged)).await {
                Ok(file) => KeyOutcome::Installed {
                    signs_with: SignsWith::beside(&location, &restored, repo).await,
                    restored,
                    file,
                },
                Err(e) => KeyOutcome::Failed {
                    error: format!(
                        "the dump's signing key {restored} could not be put in {target} \
                         ({e:#}); {}",
                        current()
                    ),
                },
            }
        }
        KeyPlan::Generate {
            key,
            staged,
            previous,
        } => {
            let generated = key.fingerprint().to_string();
            match repo
                .replace_key(*key, move || persist_key(previous, staged))
                .await
            {
                Ok(KeySwap { persisted, .. }) => KeyOutcome::Reset {
                    generated: Some(generated),
                    supplied: None,
                    file: persisted,
                },
                Err(e) => KeyOutcome::Failed {
                    error: format!(
                        "a new signing key could not replace the server's own ({e:#}); {}",
                        current()
                    ),
                },
            }
        }
        KeyPlan::Remove { path, previous } => {
            match blocking(move || remove_key(previous, path)).await {
                Ok(file) => KeyOutcome::Reset {
                    generated: None,
                    supplied: repo.key().map(|key| key.fingerprint().to_string()),
                    file,
                },
                Err(e) => KeyOutcome::Failed {
                    error: format!("the server's own signing key could not be removed ({e:#})"),
                },
            }
        }
    }
}

/// Keep what the server's own key file holds, then rename `staged` over it.
fn persist_key(previous: Previous, staged: Staged) -> anyhow::Result<KeyFile> {
    let path = staged.target().to_path_buf();
    let previous_key = previous.expected.clone();
    let kept = previous
        .keep()
        .with_context(|| format!("keeping the signing key {}", path.display()))?;
    // The copy is beside the file, so the directory synced once the file is
    // in place makes both durable.
    let installed = staged
        .install()
        .with_context(|| format!("replacing {}", path.display()))?;
    Ok(KeyFile {
        path,
        previous: previous_key,
        unsynced: unsynced(installed).or_else(|| unsynced(kept)),
    })
}

/// Keep what the server's own key file at `path` holds, then remove it.
fn remove_key(previous: Previous, path: PathBuf) -> anyhow::Result<KeyFile> {
    let previous_key = previous.expected.clone();
    let kept = previous
        .keep()
        .with_context(|| format!("keeping the signing key {}", path.display()))?;
    let removed = aurcache_common::fs::remove_synced(&path)
        .with_context(|| format!("removing {}", path.display()))?;
    Ok(KeyFile {
        path,
        previous: previous_key,
        unsynced: unsynced(removed).or_else(|| unsynced(kept)),
    })
}

/// Why a file that is in place may not survive a crash, when it may not.
fn unsynced(installed: Installed) -> Option<String> {
    match installed {
        Installed::Synced => None,
        Installed::Unsynced { error } => Some(error.to_string()),
    }
}

/// The fingerprint of the key `AURCACHE_SIGNING_KEY_FILE` supplies: the one
/// the server signs with, when it signs, and otherwise the one in the file,
/// `None` when it cannot be read.
async fn supplied_key(location: &KeyLocation, repo: &Repository) -> Option<String> {
    if let Some(key) = repo.key() {
        return Some(key.fingerprint().to_string());
    }
    let path = location.path().to_path_buf();
    blocking(move || {
        Ok(std::fs::read(&path)
            .ok()
            .and_then(|bytes| aurcache_signing::inspect(&bytes).ok())
            .map(|facts| facts.fingerprint))
    })
    .await
    .ok()
    .flatten()
}

/// Run `work` -- file I/O, key work -- on a blocking thread.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
) -> anyhow::Result<T> {
    tokio::task::spawn_blocking(work).await?
}

/// Record in the log what a restore did to the secrets that an operator has
/// to act on -- workers enroll again under a new CA, and every client trusts
/// one signing key or another -- and what could not be done.
fn record_secrets(activity: &ActivityLog, applied: &SecretsApplied) {
    match &applied.ca {
        CaOutcome::Kept | CaOutcome::Same => {}
        CaOutcome::Replaced { .. } => activity.emit(Event::RestoreCaReplaced {}),
        CaOutcome::Failed { error } => {
            tracing::error!("a restore could not put the dump's CA in place: {error}");
        }
    }
    let kept = |file: &KeyFile| file.previous.kept().map(|kept| kept.display().to_string());
    match &applied.signing_key {
        KeyOutcome::Kept => {}
        KeyOutcome::Installed {
            restored,
            signs_with,
            file,
        } => match signs_with {
            SignsWith::Restored { replaced } => {
                activity.emit(Event::RestoreSigningKeyInstalled {
                    restored: restored.clone(),
                    replaced: Some(replaced.clone()),
                    kept: kept(file),
                });
            }
            SignsWith::Nothing => activity.emit(Event::RestoreSigningKeyInstalled {
                restored: restored.clone(),
                replaced: None,
                kept: kept(file),
            }),
            SignsWith::Supplied { fingerprint, signs } => {
                activity.emit(Event::RestoreSigningKeyUnused {
                    restored: restored.clone(),
                    supplied: fingerprint.clone(),
                    signs: *signs,
                });
            }
            SignsWith::NewSigner => tracing::warn!(
                "A restore put the repository signing key {restored} in place; the server \
                 already signed with it, but through another of its subkeys, so clients have \
                 to import the key again (pacman-key --add)"
            ),
            SignsWith::Same => tracing::info!(
                "A restore put the repository signing key {restored} in place; the server \
                 already signed with it"
            ),
        },
        KeyOutcome::NotWritten {
            restored,
            supplied,
            signs,
        } => activity.emit(Event::RestoreSigningKeyUnused {
            restored: restored.clone(),
            supplied: supplied.clone(),
            signs: *signs,
        }),
        KeyOutcome::Reset {
            generated,
            supplied,
            file,
        } => activity.emit(Event::RestoreSigningKeyReset {
            removed: file.previous.fingerprint().map(str::to_string),
            generated: generated.clone(),
            supplied: supplied.clone(),
            kept: kept(file),
        }),
        KeyOutcome::Failed { error } => {
            tracing::error!("a restore could not change the signing key: {error}");
        }
    }
    // In place, but a crash soon after may undo it: said in the log alone,
    // as the restore's report says what it does, not how durably.
    if let CaOutcome::Replaced {
        unsynced: Some(error),
    } = &applied.ca
    {
        activity.emit(Event::RestoreSecretUnsynced {
            file: "the worker CA".to_string(),
            error: error.clone(),
        });
    }
    if let KeyOutcome::Installed { file, .. } | KeyOutcome::Reset { file, .. } =
        &applied.signing_key
        && let Some(error) = &file.unsynced
    {
        activity.emit(Event::RestoreSecretUnsynced {
            file: file.path.display().to_string(),
            error: error.clone(),
        });
    }
}

/// What a restore says it does to each secret it touches, should every
/// rename succeed: what the answer to it carries, from
/// [`SecretsPlan::expected`]. A secret it leaves alone has no line, so a
/// restore that takes none says nothing about secrets; one that could not be
/// put in place has none either, but a failed entry ([`secret_failures`]).
#[must_use]
pub fn secret_reports(applied: &SecretsApplied) -> Vec<SecretReport> {
    let line = |secret: Secret, change: SecretChange, detail: String| {
        Some(SecretReport {
            secret,
            change,
            detail,
        })
    };
    let ca = match &applied.ca {
        CaOutcome::Kept => None,
        CaOutcome::Same => line(
            Secret::Ca,
            SecretChange::Kept,
            "the dump's, which is this server's already: workers keep their certificates"
                .to_string(),
        ),
        CaOutcome::Replaced { .. } => line(
            Secret::Ca,
            SecretChange::Replaced,
            "the dump's, in place of this server's, which is not kept: every worker \
             enrolled under it has to enroll again"
                .to_string(),
        ),
        CaOutcome::Failed { .. } => None,
    };
    let key = match &applied.signing_key {
        KeyOutcome::Kept => None,
        KeyOutcome::NotWritten {
            restored,
            supplied,
            signs,
        } => line(
            Secret::SigningKey,
            SecretChange::Kept,
            format!(
                "the dump's key {restored} is not written: the server's own key file is the \
                 one AURCACHE_SIGNING_KEY_FILE supplies, which a restore never writes; {}",
                supplied_note(supplied.as_deref(), *signs)
            ),
        ),
        KeyOutcome::Installed {
            restored,
            signs_with,
            file,
        } => {
            let change = match file.previous {
                PreviousKey::Absent => SecretChange::Restored,
                PreviousKey::Same => SecretChange::Kept,
                PreviousKey::Kept { .. } => SecretChange::Replaced,
            };
            let signs = match signs_with {
                SignsWith::Restored { replaced } => format!(
                    "the server signs with it in place of {replaced}: clients have to trust \
                     it, and what {replaced} signed is signed again"
                ),
                SignsWith::NewSigner => "the server signs with it as before, but through \
                     another of its subkeys: clients have to import the key again to know that \
                     subkey, and what the one before signed is signed again"
                    .to_string(),
                SignsWith::Same => "the server signs with it, as before".to_string(),
                SignsWith::Supplied { fingerprint, signs } => {
                    supplied_note(fingerprint.as_deref(), *signs)
                }
                SignsWith::Nothing => {
                    "the server does not sign, and signs with it once AURCACHE_SIGN is on"
                        .to_string()
                }
            };
            line(
                Secret::SigningKey,
                change,
                format!(
                    "the dump's key {restored} in {}{}; {signs}",
                    file.path.display(),
                    previous_note(&file.previous),
                ),
            )
        }
        KeyOutcome::Reset {
            generated: Some(generated),
            file,
            ..
        } => line(
            Secret::SigningKey,
            SecretChange::Generated,
            format!(
                "a new key, {generated}, in {}{}, as a fresh install generates; the server \
                 signs with it at once, and clients have to trust it",
                file.path.display(),
                previous_note(&file.previous),
            ),
        ),
        KeyOutcome::Reset {
            generated: None,
            supplied,
            file,
        } => line(
            Secret::SigningKey,
            SecretChange::Removed,
            format!(
                "{} removed, as a fresh install has none{}; {}",
                file.path.display(),
                previous_note(&file.previous),
                supplied_note(supplied.as_deref(), supplied.is_some()),
            ),
        ),
        KeyOutcome::Failed { .. } => None,
    };
    [ca, key].into_iter().flatten().collect()
}

/// The entries a restore adds for what it could not do to the secrets once
/// the rows had committed, against the restore as a whole: each secret
/// that failed is as it was.
fn secret_failures(applied: &SecretsApplied) -> Vec<RestoreEntry> {
    let ca = match &applied.ca {
        CaOutcome::Failed { error } => Some(format!(
            "the packages were imported, but the dump's CA could not be put in place, \
             and the previous one stays: {error}"
        )),
        _ => None,
    };
    let key = match &applied.signing_key {
        KeyOutcome::Failed { error } => Some(format!("the packages were imported, but {error}")),
        _ => None,
    };
    [ca, key]
        .into_iter()
        .flatten()
        .map(|error| RestoreEntry {
            pkgbase: String::new(),
            outcome: RestoreOutcome::Failed { error },
        })
        .collect()
}

/// Where the key a restore replaced or removed is kept, for its report
/// line.
fn previous_note(previous: &PreviousKey) -> String {
    match previous {
        PreviousKey::Absent => String::new(),
        PreviousKey::Same => ", which already held it".to_string(),
        PreviousKey::Kept {
            fingerprint: Some(fingerprint),
            kept,
        } => format!(
            "; the key it held, {fingerprint}, is kept as {}",
            kept.display()
        ),
        PreviousKey::Kept {
            fingerprint: None,
            kept,
        } => format!(
            "; what it held, which does not read as a key, is kept as {}",
            kept.display()
        ),
    }
}

/// What the server signs with when it is the key `AURCACHE_SIGNING_KEY_FILE`
/// supplies, `supplied`, for a report line.
fn supplied_note(supplied: Option<&str>, signs: bool) -> String {
    let supplied = supplied.unwrap_or("a key that cannot be read");
    if signs {
        format!("the server signs with {supplied}, which AURCACHE_SIGNING_KEY_FILE supplies")
    } else {
        format!(
            "the server does not sign; it would sign with {supplied}, which \
             AURCACHE_SIGNING_KEY_FILE supplies"
        )
    }
}

/// What pass 1 did.
#[derive(Debug, Clone)]
pub struct Applied {
    pub entries: Vec<RestoreEntry>,
    /// Packages whose sources and graph the later passes must visit. A skipped
    /// package is not among them: it was already here, with its own resolution
    /// already done.
    pub touched: Vec<String>,
    /// Whether the transaction that wrote them is known to have committed,
    /// and the repository's files with it.
    pub commit: RowsCommit,
}

/// Whether the rows a restore wrote are in, and the repository's files with
/// them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowsCommit {
    /// Committed, and the repository's files are in place.
    Done,
    /// Committed, but the repository's files are not all in place, for
    /// `error` ([`FinalizationPending`]): its recorded plan finishes them at
    /// the next update or start.
    FilesPending { error: String },
    /// The COMMIT was sent and failed in a way that leaves unknown whether it
    /// took effect, for `error` ([`CommitOutcome::Unknown`]): the rows may
    /// be in or not, and the entries only say what they would be.
    Unknown { error: String },
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
    // What the transaction that committed wrote, for when finalizing the
    // repository after it fails: the rows are in, and only the files wait.
    let committed = Arc::new(std::sync::Mutex::new(None));
    let result = update
        .commit(db, |txn| {
            let dump = dump.clone();
            let options = options.clone();
            let committed = Arc::clone(&committed);
            Box::pin(async move {
                let applied = write_rows_txn(txn, &dump, &options).await?;
                *committed
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(applied.clone());
                Ok(applied)
            })
        })
        .await;
    let written = committed
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    let applied = rows_commit(result, written)?;

    // `--clear` removes every package, so every build log goes with them --
    // once the packages are known to be gone.
    if options.clear && !matches!(applied.commit, RowsCommit::Unknown { .. }) {
        crate::build_logger::remove_all_logs().await;
    }
    Ok(applied)
}

/// What committing pass 1 came to, from `result`, what the update's commit
/// returned, and `written`, what its transaction wrote before its COMMIT
/// was sent: an error only for rows known not to have committed.
fn rows_commit(
    result: anyhow::Result<Applied>,
    written: Option<Applied>,
) -> anyhow::Result<Applied> {
    let error = match result {
        Ok(applied) => return Ok(applied),
        Err(error) => error,
    };
    let Some(commit) = error
        .downcast_ref::<FinalizationPending>()
        .map(|pending| pending.commit)
    else {
        return Err(error);
    };
    let Some(applied) = written else {
        return Err(error);
    };
    let error = format!("{error:#}");
    Ok(Applied {
        commit: match commit {
            CommitOutcome::Committed => RowsCommit::FilesPending { error },
            CommitOutcome::Unknown => RowsCommit::Unknown { error },
        },
        ..applied
    })
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
    Ok(Applied {
        entries,
        touched,
        commit: RowsCommit::Done,
    })
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
            arches: Set(WorkerList::new(&worker.arches)),
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

/// A dump's build flags in their canonical form; [`load_dump`] has refused
/// one a build cannot take.
fn build_flags(package: &DumpPackage) -> BuildFlags {
    BuildFlags::new(
        aurcache_common::build_flags::canonical(&package.build_flags).unwrap_or_default(),
    )
}

/// The columns a dump owns. Everything else on the row is derived and is left
/// to the passes that follow.
fn apply_config(active: &mut packages::ActiveModel, package: &DumpPackage, patch: Option<String>) {
    active.platforms = Set(platforms(package));
    active.build_flags = Set(build_flags(package));
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
    let mut active = packages::ActiveModel {
        name: Set(pkgbase.to_string()),
        // Nothing is built for it here yet, which is what the add path
        // records for a new package too.
        status: Set(BuildState::Enqueued),
        out_of_date: Set(false),
        ..Default::default()
    };
    apply_config(&mut active, package, patch);
    Ok(active.insert(db).await?.id)
}

/// Settings, keyed back from pkgbase onto row ids.
///
/// Only the settings this version knows, and a package value only for one a
/// package may override: anything else is a row nothing would read.
async fn write_settings<C: sea_orm::ConnectionTrait>(
    db: &C,
    dump: &LoadedDump,
    ids: &BTreeMap<String, i32>,
) -> anyhow::Result<()> {
    let change = |pkg_id: Option<i32>, key: &str, value: &str| {
        let setting = Setting::from_key(key)?;
        (pkg_id.is_none() || setting.meta().scope == Scope::Package).then(|| Change {
            setting,
            pkg_id,
            value: Some(value.to_string()),
        })
    };
    let mut changes: Vec<Change> = dump
        .settings
        .global
        .iter()
        .filter_map(|(key, value)| change(None, key, value))
        .collect();
    for (pkgbase, values) in &dump.settings.packages {
        // Validation guarantees the package is in the dump; this only skips one
        // that was left alone by `Skip`, whose own settings are already right.
        let Some(&id) = ids.get(pkgbase) else {
            continue;
        };
        changes.extend(
            values
                .iter()
                .filter_map(|(key, value)| change(Some(id), key, value)),
        );
    }
    match crate::settings::write(db, changes).await? {
        crate::settings::Write::Written => Ok(()),
        crate::settings::Write::GlobalOnly(setting) => anyhow::bail!(
            "{} applies to the whole server and cannot be set per package",
            setting.meta().key
        ),
    }
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
    let SourceFacts { sourceinfo, deps } = pkg_deps(
        store,
        &row.source_data,
        row.patch.as_deref(),
        &row.platforms,
    )
    .await?;

    record_source_names(db, row.id, pkgbase, &deps).await?;
    packages::ActiveModel {
        id: Set(row.id),
        upstream_version: Set(Some(sourceinfo.base.version.to_string())),
        ..Default::default()
    }
    .update(db)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Applied, RowsCommit, clear_existing, load_dump, rows_commit};
    use crate::repository::FinalizationPending;
    use aurcache_common::api::dump::{DUMP_SCHEMA_VERSION, RestoreEntry, RestoreOutcome};
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

    /// makepkg is handed a package's flags as they are, so one it rejects
    /// would fail every build of the restored package.
    #[test]
    fn a_flag_no_build_can_take_is_refused() {
        let mut files = good();
        files.insert(
            "packages.json",
            r#"{"hello":{"source_data":{"type":"aur","name":"hello"},
                "platforms":["x86_64"],"build_flags":["--pgpfetch"],"directly_requested":true}}"#
                .to_string(),
        );
        let error = load(files).unwrap_err().to_string();
        assert!(error.contains("'--pgpfetch'"), "unhelpful: {error}");
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
        use aurcache_db::packages::SourceData;
        use sea_orm::{ActiveModelTrait, EntityTrait, Set, TransactionTrait};

        let db = aurcache_db::testing::memory_db().await;
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
            r#"[{"name":"w","cert_fingerprint":"fp","status":"approved","arches":["x86_64"],
                 "package_affinity":[],"priority":0,"concurrency":1,
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
        // Printing it says it carries secrets, and not what they are.
        let printed = format!("{loaded:?}");
        assert!(printed.contains("DumpSecrets"), "{printed}");
        assert!(
            !printed.contains("\"key\"") && !printed.contains("abc"),
            "{printed}"
        );
        let secrets = loaded.secrets.expect("secrets were dropped");
        assert_eq!(secrets.ca_key_pem, "key");
        assert_eq!(secrets.tokens.len(), 1);
        assert_eq!(secrets.tokens[0].username, "alice");
    }

    /// What pass 1 wrote before its COMMIT was sent.
    fn written() -> Applied {
        Applied {
            entries: vec![RestoreEntry {
                pkgbase: "hello".to_string(),
                outcome: RestoreOutcome::Imported,
            }],
            touched: vec!["hello".to_string()],
            commit: RowsCommit::Done,
        }
    }

    /// Rows that committed while the repository's files wait are imported;
    /// rows whose COMMIT may or may not have taken effect are neither
    /// imported nor refused, and say so; any other error is a refusal.
    #[test]
    fn a_commit_is_told_from_one_whose_outcome_is_unknown() {
        let pending = |error: FinalizationPending| Err(anyhow::Error::new(error));

        let applied = rows_commit(
            pending(FinalizationPending::committed(anyhow::anyhow!(
                "a rename failed"
            ))),
            Some(written()),
        )
        .unwrap();
        assert!(
            matches!(&applied.commit, RowsCommit::FilesPending { error } if error.contains("a rename failed")),
            "{applied:?}"
        );
        assert_eq!(applied.touched, ["hello"]);

        let applied = rows_commit(
            pending(FinalizationPending::unknown(anyhow::anyhow!(
                "the connection was lost"
            ))),
            Some(written()),
        )
        .unwrap();
        assert!(
            matches!(&applied.commit, RowsCommit::Unknown { error } if error.contains("the connection was lost")),
            "{applied:?}"
        );

        assert!(
            rows_commit(
                Err(anyhow::anyhow!("the database said no")),
                Some(written())
            )
            .is_err()
        );
        assert_eq!(
            rows_commit(Ok(written()), None).unwrap().commit,
            RowsCommit::Done
        );
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
