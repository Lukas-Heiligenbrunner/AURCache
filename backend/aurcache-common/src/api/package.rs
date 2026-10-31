use crate::source::{GitSourceSpec, SourceData};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use utoipa::ToSchema;

#[derive(Deserialize, Serialize, ToSchema, Clone)]
pub struct AddPackage {
    pub platforms: Option<Vec<String>>,
    pub build_flags: Option<Vec<String>>,
    pub source: SourceData,
    /// Optional initial patch, expressed as full file contents (path -> new
    /// content) rather than a diff - the backend diffs each entry against
    /// the source's pristine content itself. Lets a package that fails to
    /// parse upstream (e.g. a malformed PKGBUILD) be fixed up and added in
    /// one step, instead of having to add it broken and edit it afterwards.
    pub patched_files: Option<BTreeMap<String, String>>,
}

/// Ask for a package's update check, and a build when it finds one.
#[derive(Deserialize, Serialize, ToSchema, Clone, Debug, Default)]
pub struct UpdatePackage {
    /// Build even when the version did not change.
    pub force: bool,
}

#[derive(Serialize, Deserialize, ToSchema)]
pub struct SourceFileList {
    pub files: Vec<String>,
    /// The files the package's stored patch changes, so a file list can mark
    /// them without opening each one. Empty for a source that is not a package
    /// yet, which has no patch.
    pub patched: Vec<String>,
}

/// Effective content of a single source file for an already-added package.
///
/// The pristine content is always included so the UI can fall back to it
/// (and offer a "revert" action) even if the stored patch no longer applies
/// cleanly to the current upstream source.
#[derive(Serialize, Deserialize, ToSchema)]
pub struct SourceFileContent {
    pub path: String,
    pub original_content: String,
    /// Content with the stored patch applied, if this file is part of the
    /// patch and it still applies cleanly. `None` if there's no patch for
    /// this file, or if the stored diff no longer applies (see
    /// `patch_error`) - in that case the UI should fall back to
    /// `original_content`.
    pub patched_content: Option<String>,
    /// Set if this file is part of the stored patch but applying it failed
    /// (e.g. upstream changed enough that the diff's context no longer
    /// matches).
    pub patch_error: Option<String>,
    /// The stored unified diff for this file as it was authored, so the UI can
    /// show what the patch does even when it can no longer be applied. `None`
    /// if this file isn't part of the patch.
    pub stored_patch: Option<String>,
}

#[derive(Deserialize, Serialize, ToSchema)]
pub struct SourceFileUpdate {
    pub path: String,
    pub content: String,
}

/// Request body for the pre-add source preview endpoints: identifies a
/// not-yet-added source so its (pristine) files can be listed/read before
/// `POST /package` is ever called.
#[derive(Deserialize, Serialize, ToSchema, Clone)]
pub struct SourcePreviewRequest {
    pub source: SourceData,
}

/// Request body to read a single pristine file of a not-yet-added source.
#[derive(Deserialize, Serialize, ToSchema, Clone)]
pub struct SourcePreviewFileRequest {
    pub source: SourceData,
    pub path: String,
}

/// A partial update of a package: every field left `None` is left as it is.
///
/// Absent fields are not sent, so a client builds one from `Default` and sets
/// only what it changes. Only what an operator configures is here: the name,
/// status and versions belong to the add, build and version-check flows, and a
/// package renamed from outside would lose its logs, artifacts and edges.
#[derive(Deserialize, ToSchema, Serialize, Default, Clone, Debug, PartialEq, Eq)]
pub struct PackagePatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_flags: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platforms: Option<Vec<String>>,
    /// The source patch, as stored: a JSON object of one unified diff per
    /// file, applied on top of the fetched source. `Some(None)` clears an
    /// existing patch, `None` leaves it untouched.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "clearable"
    )]
    pub patch: Option<Option<String>>,
}

/// A field where `null` means "clear it" rather than "not given": absent is
/// `None` (through `#[serde(default)]`), `null` is `Some(None)`.
///
/// Serde's own reading of `Option<Option<T>>` folds `null` into the outer
/// `None`, which made a clear indistinguishable from leaving the field alone.
fn clearable<'de, T, D>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

#[derive(Deserialize, ToSchema, Serialize, Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "db", derive(sea_orm::FromQueryResult))]
pub struct SimplePackage {
    pub name: String,
    /// False for a package that is only here because something else needs it.
    ///
    /// The list leaves those out by default, so this is what tells the two
    /// apart once they are asked for together.
    pub directly_requested: bool,
    pub status: crate::build_state::BuildState,
    pub out_of_date: bool,
    pub built_version: Option<String>,
    /// `None` until a version check has determined it. The column is nullable
    /// and rows land in this list before their first check — the dependency
    /// migration inserts them without one, and adding such a package
    /// explicitly only flips `directly_requested`. Typed as a plain `String`
    /// this failed to decode, which took down the whole route, not one row.
    pub upstream_version: Option<String>,
    /// Combined size in bytes of every artifact this package has in the
    /// repository.
    ///
    /// `None` when there is nothing to total: the package has never built, or
    /// at least one of its artifacts has no recorded size. Deliberately not a
    /// partial sum -- a total smaller than the files it claims to cover reads
    /// as a bug rather than as missing data. Matches the total the package page
    /// shows for the same package.
    pub total_size: Option<i64>,
}

#[derive(Deserialize, ToSchema, Serialize, Clone, Debug, PartialEq, Eq)]
pub struct ExtendedPackage {
    pub name: String,
    pub directly_requested: bool,
    pub status: crate::build_state::BuildState,
    pub out_of_date: bool,
    pub built_version: Option<String>,
    pub selected_platforms: Vec<String>,
    pub selected_build_flags: Option<Vec<String>>,
    /// `None` while it is still unknown, matching [`SimplePackage`]. Coercing
    /// it to `""` here would make one field mean two things depending on the
    /// route, which is what it used to do.
    pub upstream_version: Option<String>,
    pub package_source: PackageSource,
    pub split_packages: Option<Vec<String>>,
    /// The built artifacts currently in the repository for this package: one
    /// per split package per platform, newest build only. Empty until the
    /// package has built successfully at least once.
    pub files: Vec<PackageFile>,
    pub dependencies: Vec<PackageDependency>,
    pub dependents: Vec<PackageDependency>,
    /// Whether the package currently has a source patch applied.
    pub has_patch: bool,
    // Read from the package's source checkout rather than from the AUR, so
    // these describe a git-sourced package as well as an AUR one, and reflect
    // the patched PKGBUILD — which is what actually gets built.
    pub description: Option<String>,
    pub project_url: Option<String>,
    /// Several licenses are joined with ", ".
    pub licenses: Option<String>,
    /// From the `# Maintainer:` comment in the PKGBUILD, which is where the
    /// name lives — `.SRCINFO` has no such field.
    pub maintainer: Option<String>,
    /// Unix seconds of the packaging repository's first commit.
    pub first_submitted: Option<i64>,
    /// Unix seconds of its newest commit: when the packaging was last touched,
    /// not when the upstream project last changed.
    pub last_modified: Option<i64>,
}

/// Request to add several packages in one go.
///
/// Separate from [`AddPackage`] rather than a list of them because the point is
/// what the server can do once it sees the whole set: every AUR name is
/// resolved to its pkgbase in one batched RPC request instead of one per
/// package. Targeting is shared across the batch -- a restore applies the same
/// platforms and flags to everything it puts back.
#[derive(Deserialize, ToSchema, Serialize, Clone, Debug, PartialEq, Eq)]
pub struct AddPackages {
    pub platforms: Option<Vec<String>>,
    pub build_flags: Option<Vec<String>>,
    pub sources: Vec<SourceData>,
}

/// What starting a bulk add returns, immediately.
///
/// The work is not done when this is sent -- it has barely started. Poll
/// [`BulkAddProgress`] with the id to watch it, or do not: the job does not
/// depend on anyone watching.
#[derive(Deserialize, ToSchema, Serialize, Clone, Debug, PartialEq, Eq)]
pub struct BulkAddAccepted {
    pub job_id: i32,
    /// How many sources were taken on. Not how many will succeed.
    pub accepted: i32,
}

/// How one package in a bulk add turned out.
#[derive(Deserialize, ToSchema, Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case", tag = "outcome")]
pub enum BulkAddOutcome {
    /// Added, along with any dependencies it pulled in.
    Added,
    /// Already present, so nothing to do. Marked directly-requested if it had
    /// only been here as a dependency.
    Existed,
    /// Not added. The rest of the batch continued regardless.
    Failed { error: String },
}

/// One line of a bulk add's progress.
#[derive(Deserialize, ToSchema, Serialize, Clone, Debug, PartialEq, Eq)]
pub struct BulkAddEntry {
    /// The source as the caller named it, so a failure can be matched back to
    /// the request even when the name never resolved to a pkgbase.
    pub name: String,
    /// The pkgbase the source turned out to be, once it is known.
    ///
    /// Separate from `name` because they are not the same thing and only one of
    /// them can be linked to: `name` is whatever was typed -- an AUR package
    /// name that may differ from its base, or a git URL -- while this is the row
    /// the package now has. `None` for anything that never got that far, which
    /// is every failure that happened during resolution.
    pub pkgbase: Option<String>,
    #[serde(flatten)]
    pub outcome: BulkAddOutcome,
}

/// A bulk add's state, and the entries after the offset the caller asked from.
#[derive(Deserialize, ToSchema, Serialize, Clone, Debug, PartialEq, Eq)]
pub struct BulkAddProgress {
    pub id: i32,
    pub total: i32,
    pub completed: i32,
    pub failed: i32,
    /// Whether the job has stopped -- successfully or not. A job whose server
    /// restarted mid-run is finished too, with the unreached packages recorded
    /// as failures rather than left pending forever.
    pub finished: bool,
    /// Entries from the requested offset onwards, so a caller polling only ever
    /// receives what it has not already seen.
    pub entries: Vec<BulkAddEntry>,
}

/// One built artifact in the repository.
#[derive(Deserialize, ToSchema, Serialize, Clone, Debug, PartialEq, Eq)]
pub struct PackageFile {
    /// The artifact's filename, which is also its path under the platform's
    /// repository directory.
    pub filename: String,
    pub platform: String,
    /// Size on disk in bytes -- the compressed download size, the same figure
    /// pacman reports as `%CSIZE%`.
    ///
    /// `None` when it is not known: the row predates the size column and the
    /// file was gone by the time the startup backfill looked for it. Rendered
    /// as unknown rather than as zero.
    pub size: Option<i64>,
}

#[derive(Deserialize, ToSchema, Serialize, Clone, Debug, PartialEq, Eq)]
pub struct PackageDependency {
    pub name: String,
    /// What this relation requires, e.g. `>=1.3`. Empty when unconstrained.
    pub version_constraint: String,
    /// Build state of the dependency itself.
    pub status: crate::build_state::BuildState,
    /// The version currently in the repository — the dependency's newest
    /// successful build. `None` when it has never built.
    pub built_version: Option<String>,
    /// Whether [`Self::built_version`] satisfies [`Self::version_constraint`].
    ///
    /// This is what decides whether a dependency is holding a build back: the
    /// builder promotes a dependent only once every dependency has a
    /// successful build whose version satisfies the recorded constraint. False
    /// here means this relation is the thing blocking.
    pub satisfied: bool,
}

#[derive(Deserialize, ToSchema, Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "package_type", rename_all = "PascalCase")]
pub enum PackageSource {
    Aur(AurPackage),
    AurNotFound(AurNotFoundPackage),
    Git(GitSourceSpec),
    Upload(UploadPackage),
}

// todo upload package
#[derive(Deserialize, ToSchema, Serialize, Default, Clone, Debug, PartialEq, Eq)]
pub struct UploadPackage {}

#[derive(Deserialize, ToSchema, Serialize, Default, Clone, Debug, PartialEq, Eq)]
pub struct AurNotFoundPackage {}

#[derive(Deserialize, ToSchema, Serialize, Default, Clone, Debug, PartialEq, Eq)]
pub struct AurPackage {
    pub name: String,
    /// Whether the AUR reports the package as flagged out of date by a user.
    ///
    /// The one thing here that only the AUR knows: everything else a package
    /// page shows is read from its source checkout, which also covers
    /// git-sourced packages that have no AUR entry at all.
    pub aur_flagged_outdated: bool,
    pub aur_url: String,
}

/// Where a replacement candidate was found.
#[derive(Deserialize, ToSchema, Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CandidateSource {
    /// Already tracked by this instance.
    Tracked,
    /// Would be added from the AUR and built.
    Aur,
}

/// Whether a candidate meets what the edge being replaced recorded.
#[derive(Deserialize, ToSchema, Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReplacementVerdict {
    /// Its version meets the constraint, or there is no constraint.
    Satisfied,
    /// Not decidable here rather than negative: nothing has been built for it
    /// yet, so there is no version to check. The build queue re-checks the
    /// constraint against each real build, so this settles itself later.
    Unknown,
    /// The version it is known to be at fails the constraint.
    Unsatisfied,
}

/// One dependency edge, and what could take its place.
#[derive(Deserialize, ToSchema, Serialize, Clone, Debug, PartialEq, Eq)]
pub struct DependencyOptions {
    /// The package whose dependency this is.
    pub dependent: String,
    /// The package it currently depends on.
    pub current: String,
    /// The names the dependent declares that `current` answers to. Normally
    /// one; more where a single package covers several of them.
    ///
    /// The edge records the constraint but not the name it came from, so this
    /// is recovered by reading the dependent's source. It is what the
    /// candidates are searched for.
    pub declared_names: Vec<String>,
    /// What the edge requires, e.g. `>=1.3`. Empty when unconstrained.
    pub version_constraint: String,
    /// Declared names the official repositories publish.
    ///
    /// When they cover every one of them the edge can be dropped outright
    /// rather than repointed: pacman resolves it at install time and AURCache
    /// has nothing left to build. Rare, and it happens when a name moved into
    /// an official repository after the dependent was last resolved.
    pub official: Vec<String>,
    /// What could stand in, best first, tracked packages before AUR ones.
    /// Never includes the current dependency or the dependent itself.
    pub candidates: Vec<DependencyCandidate>,
    /// Why the AUR could not be searched, when it could not be.
    ///
    /// `candidates` then holds only what is already tracked, which is not the
    /// same as the AUR having nothing to offer. Saying "nothing else provides
    /// it" when the truth is "could not ask" is how someone ends up removing a
    /// dependency they could have replaced, so the two are kept apart here as
    /// they are everywhere else resolution asks a source a question.
    pub aur_error: Option<String>,
}

/// One package that could take over a dependency edge.
#[derive(Deserialize, ToSchema, Serialize, Clone, Debug, PartialEq, Eq)]
pub struct DependencyCandidate {
    pub pkgbase: String,
    pub source: CandidateSource,
    /// The version it is known to be at: its newest successful build if it is
    /// tracked, or what the AUR publishes. `None` when nothing has been built
    /// for a tracked package yet.
    pub version: Option<String>,
    pub verdict: ReplacementVerdict,
}

/// Point a dependency edge at something else.
#[derive(Deserialize, ToSchema, Serialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct ReplaceDependency {
    /// The package base to depend on instead. `None` drops the edge, which is
    /// accepted only when the official repositories publish every declared
    /// name behind it.
    pub replacement: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::PackagePatch;

    /// Absent leaves a field alone, `null` clears it, and a patch built from
    /// `Default` sends nothing it did not set.
    #[test]
    fn a_patch_tells_absent_from_cleared() {
        let absent: PackagePatch = serde_json::from_str("{}").unwrap();
        assert_eq!(absent.patch, None);
        let cleared: PackagePatch = serde_json::from_str(r#"{"patch": null}"#).unwrap();
        assert_eq!(cleared.patch, Some(None));
        let set: PackagePatch = serde_json::from_str(r#"{"patch": "diff"}"#).unwrap();
        assert_eq!(set.patch, Some(Some("diff".to_string())));

        let only_platforms = PackagePatch {
            platforms: Some(vec!["x86_64".to_string()]),
            ..PackagePatch::default()
        };
        assert_eq!(
            serde_json::to_string(&only_platforms).unwrap(),
            r#"{"platforms":["x86_64"]}"#
        );
        let clearing = PackagePatch {
            patch: Some(None),
            ..PackagePatch::default()
        };
        assert_eq!(
            serde_json::to_string(&clearing).unwrap(),
            r#"{"patch":null}"#
        );
    }
}
