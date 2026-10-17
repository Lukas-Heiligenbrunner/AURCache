//! The lite export format: what a human authored, not what `AURCache` worked out.
//!
//! An instance's interesting state is small -- which packages you asked for,
//! how you configured them, which workers you trust. Build history, logs,
//! resolved dependency graphs and the packages themselves are large,
//! reproducible, and left out; the next resolve or build rebuilds them.
//!
//! Nothing here refers to a package by row id. Everything keys on pkgbase,
//! including settings, which the database keys by `pkg_id`. That removes the
//! id-remapping an import would otherwise need, and keeps a dump diffable and
//! mergeable by hand.

use crate::source::SourceData;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use utoipa::ToSchema;

/// The format version an importer checks before reading anything else.
///
/// The point of the format is restoring into a *different* `AURCache` version,
/// so an importer must refuse a dump newer than it understands rather than
/// guess at fields it has never seen.
pub const DUMP_SCHEMA_VERSION: u32 = 1;

/// File names within the archive. Named here so the writer and the reader
/// cannot disagree about them.
pub const MANIFEST_FILE: &str = "manifest.json";
pub const PACKAGES_FILE: &str = "packages.json";
pub const SETTINGS_FILE: &str = "settings.json";
pub const WORKERS_FILE: &str = "workers.json";
/// Directory holding one `<pkgbase>.patch` per patched package.
pub const PATCH_DIR: &str = "patches";
/// The CA certificate, present only in a dump taken with secrets.
pub const CA_CERT_FILE: &str = "ca-cert.pem";
/// The CA private key. The most dangerous thing a dump can contain: it signs
/// worker identities, so anyone holding it can mint a certificate this server
/// accepts as a worker.
pub const CA_KEY_FILE: &str = "ca-key.pem";
/// API token hashes, present only in a dump taken with secrets.
pub const TOKENS_FILE: &str = "tokens.json";

#[derive(Serialize, Deserialize, ToSchema, Debug, Clone, PartialEq, Eq)]
pub struct DumpManifest {
    /// Checked first, and refused if newer than the importer understands.
    pub schema_version: u32,
    /// Which `AURCache` wrote it. Informational: the schema version is what
    /// decides compatibility.
    pub aurcache_version: String,
    /// Unix seconds.
    pub created_at: i64,
    /// Whether the archive carries the CA, worker certificates and token
    /// hashes. A dump with this set is dangerous to share -- the CA private
    /// key mints worker identities the server accepts.
    pub includes_secrets: bool,
}

/// One package, as its owner configured it.
///
/// Everything `AURCache` derives is absent: status, versions, the resolved
/// dependency graph, split package names, provides. A restore rebuilds those.
#[derive(Serialize, Deserialize, ToSchema, Debug, Clone, PartialEq, Eq)]
pub struct DumpPackage {
    pub source_data: SourceData,
    /// Expanded from the database's semicolon-delimited column, because a dump
    /// is meant to be read and edited by hand.
    pub platforms: Vec<String>,
    pub build_flags: Vec<String>,
    /// False for a package present only because something else needs it. Kept
    /// because a restore that made every package directly-requested would turn
    /// dependencies into things the user has to manage.
    pub directly_requested: bool,
    /// Whether `patches/<pkgbase>.patch` accompanies this entry.
    ///
    /// Not serialised: it is derived from whether the file is there. A flag and
    /// a file can disagree; a file cannot disagree with itself.
    #[serde(skip)]
    pub has_patch: bool,
}

/// Settings, split by what they apply to.
#[derive(Serialize, Deserialize, ToSchema, Debug, Clone, PartialEq, Eq, Default)]
pub struct DumpSettings {
    /// Server-wide values, by setting key.
    pub global: BTreeMap<String, String>,
    /// Per-package overrides, by pkgbase then setting key.
    pub packages: BTreeMap<String, BTreeMap<String, String>>,
}

/// A worker the server trusts, and how work is routed to it.
///
/// The fingerprint is the identity: a worker whose certificate is not in the
/// dump re-enrolls on first contact and is auto-approved because its
/// fingerprint is already known.
#[derive(Serialize, Deserialize, ToSchema, Debug, Clone, PartialEq, Eq)]
pub struct DumpWorker {
    pub name: String,
    pub cert_fingerprint: String,
    /// Always `approved`: nothing else is dumped, and a restore approves
    /// every worker it brings back.
    pub status: crate::api::worker::ApprovalStatus,
    pub native_arches: Vec<String>,
    pub emulated_arches: Vec<String>,
    pub package_affinity: Vec<String>,
    pub priority: i32,
    pub concurrency: i32,
    /// The values set for this worker's settings on the server, by key.
    ///
    /// Configuration someone chose, which is what a dump is for. Absent from a
    /// dump written before workers could be configured here, which restores as
    /// none set.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub settings: BTreeMap<String, String>,
    /// The certificate this worker holds, present only in a dump taken with
    /// secrets.
    ///
    /// Only meaningful under the CA that signed it, so it never travels without
    /// one -- a dump carrying certificates and no CA would look coherent and
    /// authenticate nobody.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signed_cert: Option<String>,
    /// When that certificate expires. Unix seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_after: Option<i64>,
}

/// One API token, as the database holds it.
///
/// The hash, never the token. `api_tokens` only ever stores a SHA-256 digest,
/// so a dump carries nothing directly usable to authenticate -- but restoring
/// it means the tokens users already hold keep working, which is the point.
#[derive(Serialize, Deserialize, ToSchema, Debug, Clone, PartialEq, Eq)]
pub struct DumpToken {
    pub username: String,
    pub token_hash: String,
}

/// `packages.json`: pkgbase to package. A map rather than a list so a dump
/// diffs cleanly when one entry changes.
pub type DumpPackages = BTreeMap<String, DumpPackage>;

/// What a dump carries that is dangerous to hold.
///
/// The CA certificate and its private key travel together with the worker
/// certificates, as one unit: a signed certificate is only meaningful under the
/// CA that signed it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DumpSecrets {
    pub ca_cert_pem: String,
    pub ca_key_pem: String,
    pub tokens: Vec<DumpToken>,
}

/// What to do about secrets a dump carries.
///
/// Defaults to leaving them alone, and not for symmetry with the package
/// policy: replacing a CA invalidates every certificate the current workers
/// hold, which is a destructive act in the mode whose whole promise is that it
/// only adds.
#[derive(Serialize, Deserialize, ToSchema, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum SecretsPolicy {
    /// Leave this instance's CA and tokens as they are.
    #[default]
    Ignore,
    /// Take the dump's, replacing what is here.
    Copy,
}

/// What to do about a package the dump carries that already exists here.
///
/// Three policies rather than one flag because the right answer depends on why
/// you are importing: topping up an instance from a colleague's dump wants
/// `Skip`, rebuilding one from your own backup wants `Overwrite`.
#[derive(Serialize, Deserialize, ToSchema, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ExistingPackagePolicy {
    /// Leave what is here alone. The safe default: an import that only ever
    /// adds cannot destroy configuration nobody meant to replace.
    #[default]
    Skip,
    /// Replace its configuration with the dump's.
    Overwrite,
    /// Leave its configuration alone, but adopt the dump's patch when it has
    /// none of its own.
    ///
    /// Nothing is merged textually. Two patches against the same PKGBUILD
    /// cannot be combined without understanding both, so when each side has one
    /// the import is refused rather than picking -- that is a conflict only the
    /// operator can resolve, and guessing produces a package that builds
    /// something nobody wrote.
    MergePatches,
}

impl SecretsPolicy {
    /// The spelling the restore route's query string takes.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ignore => "ignore",
            Self::Copy => "copy",
        }
    }
}

impl std::str::FromStr for SecretsPolicy {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "ignore" => Ok(Self::Ignore),
            "copy" => Ok(Self::Copy),
            other => Err(format!("unknown secrets policy '{other}'")),
        }
    }
}

impl ExistingPackagePolicy {
    /// The spelling the restore route's query string takes.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Skip => "skip",
            Self::Overwrite => "overwrite",
            Self::MergePatches => "merge-patches",
        }
    }
}

impl std::str::FromStr for ExistingPackagePolicy {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "skip" => Ok(Self::Skip),
            "overwrite" => Ok(Self::Overwrite),
            "merge-patches" => Ok(Self::MergePatches),
            other => Err(format!("unknown on_existing policy '{other}'")),
        }
    }
}

/// How an import should behave.
#[derive(Serialize, Deserialize, ToSchema, Debug, Clone, Default)]
pub struct RestoreOptions {
    /// Report what would happen and change nothing.
    #[serde(default)]
    pub dry_run: bool,
    #[serde(default)]
    pub on_existing: ExistingPackagePolicy,
    /// Replace, rather than add to, whatever the dump covers.
    ///
    /// The rule is that the dump replaces what it *contains*: packages,
    /// settings and workers all travel in every dump, so all three are wiped
    /// and rewritten. Anything a dump does not carry is untouched by this --
    /// which is what keeps a public dump from destroying a CA it never had.
    ///
    /// `on_existing` has nothing left to decide once this is set: there is no
    /// existing package by the time the dump is written.
    #[serde(default)]
    pub clear: bool,
    /// What to do about the CA, worker certificates and token hashes, when the
    /// dump carries any. Ignored when it does not.
    #[serde(default)]
    pub secrets: SecretsPolicy,
}

/// What an import did, or would do, to one package.
#[derive(Serialize, Deserialize, ToSchema, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case", tag = "outcome")]
pub enum RestoreOutcome {
    /// Not here before; inserted.
    Imported,
    /// Already here and left alone.
    Skipped,
    /// Already here; its configuration was replaced.
    Overwritten,
    /// Already here and left as it was, except that it had no patch and the
    /// dump had one.
    PatchAdopted,
    /// Rejected. The rest of the import still applied.
    Failed { error: String },
}

impl RestoreOutcome {
    /// The outcome in one word, as a report line shows it.
    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Imported => "imported",
            Self::Skipped => "skipped",
            Self::Overwritten => "overwritten",
            Self::PatchAdopted => "patched",
            Self::Failed { .. } => "failed",
        }
    }
}

/// One line of an import's report.
#[derive(Serialize, Deserialize, ToSchema, Debug, Clone, PartialEq, Eq)]
pub struct RestoreEntry {
    pub pkgbase: String,
    #[serde(flatten)]
    pub outcome: RestoreOutcome,
}

/// An import's state and the entries after the offset asked for.
#[derive(Serialize, Deserialize, ToSchema, Debug, Clone, PartialEq, Eq)]
pub struct RestoreProgress {
    pub id: i32,
    pub total: i32,
    pub completed: i32,
    pub failed: i32,
    pub finished: bool,
    pub entries: Vec<RestoreEntry>,
}

/// What starting an import returns.
///
/// A dry run has no id to poll: it changed nothing, so there is nothing to
/// watch, and the entries it would have written are in `preview`.
#[derive(Serialize, Deserialize, ToSchema, Debug, Clone, PartialEq, Eq)]
pub struct RestoreAccepted {
    pub job_id: Option<i32>,
    pub total: i32,
    /// Populated only for a dry run.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub preview: Vec<RestoreEntry>,
}

#[cfg(test)]
mod tests {
    use super::{ExistingPackagePolicy, SecretsPolicy};

    /// Every policy parses back from the spelling it is sent as.
    #[test]
    fn policies_round_trip_through_the_query_string() {
        for policy in [
            ExistingPackagePolicy::Skip,
            ExistingPackagePolicy::Overwrite,
            ExistingPackagePolicy::MergePatches,
        ] {
            assert_eq!(policy.as_str().parse(), Ok(policy));
        }
        for policy in [SecretsPolicy::Ignore, SecretsPolicy::Copy] {
            assert_eq!(policy.as_str().parse(), Ok(policy));
        }
        assert!("merge_patches".parse::<ExistingPackagePolicy>().is_err());
    }
}
