//! `packages`: adding, updating, patching and removing packages.

use crate::builds::{WatchScope, follow_trigger, list_scoped_builds};
use crate::cli::{
    AddPackageArgs, DependencyCommand, ListPackagesArgs, OutputFormat, PackagesCommand, PatchArg,
    PatchPackageArgs, UpdatePackageArgs,
};
use crate::output::{
    Progress, bool_label, follow_job, join_or_dash, option_text, print_done_message, print_table,
    render,
};
use crate::{config, pacman};
use anyhow::{Context, Result, anyhow, bail};
use aurcache_client::{
    AddPackageRequest, AddPackagesRequest, AurCacheClient, BulkAddAccepted, BulkAddEntry,
    BulkAddOutcome, BulkAddProgress, CandidateSource, DependencyOptions, ExtendedPackage,
    GitSourceSpec, PackageDependency, PackagePatch, PackageSource, ReplacementVerdict,
    SimplePackage, SourceData, UpdatePackage, looks_like_git_url,
};
use dialoguer::Confirm;
use serde::Serialize;
use std::collections::{BTreeMap, HashSet};

pub(crate) async fn run_packages_command(
    client: &AurCacheClient,
    format: OutputFormat,
    command: PackagesCommand,
) -> Result<()> {
    match command {
        PackagesCommand::List(args) => render_packages_list(client, format, args).await,
        PackagesCommand::Get { pkgbase } => render_package(client, format, &pkgbase).await,
        PackagesCommand::Add(args) => add_package_command(client, format, args).await,
        PackagesCommand::Update(args) => update_package_command(client, format, args).await,
        PackagesCommand::Patch(args) => patch_package_command(client, format, args).await,
        PackagesCommand::Dep { command } => run_dependency_command(client, format, command).await,
        PackagesCommand::Rm { pkgbases } => {
            remove_packages_command(client, format, &pkgbases).await
        }
    }
}

pub(crate) async fn render_packages_list(
    client: &AurCacheClient,
    format: OutputFormat,
    args: ListPackagesArgs,
) -> Result<()> {
    let packages = client
        .list_packages(&aurcache_client::PackageQuery {
            limit: args.limit,
            page: args.page,
            dependencies: if args.all {
                aurcache_client::DependencySelection::Include
            } else {
                aurcache_client::DependencySelection::RequestedOnly
            },
        })
        .await?;
    if args.quiet {
        let names = packages
            .into_iter()
            .map(|package| package.name)
            .collect::<Vec<_>>();
        return render(format, &names, |names| {
            for name in names {
                println!("{name}");
            }
        });
    }
    render(format, &packages, |packages| print_package_list(packages))
}

pub(crate) async fn render_package(
    client: &AurCacheClient,
    format: OutputFormat,
    pkgbase: &str,
) -> Result<()> {
    let package = client.get_package(pkgbase).await?;
    render(format, &package, print_package)
}

/// Show what `--from-installed` found, and get a yes before submitting it.
///
/// A machine with a long AUR history produces a long list, and adding it is not
/// a quiet operation: every entry resolves its dependencies and enqueues builds
/// for them. So the list is printed and confirmed rather than acted on from one
/// flag.
pub(crate) fn confirm_bulk_add(packages: &[String], yes: bool) -> Result<()> {
    println!("{} package(s) to add:", packages.len());
    for name in packages {
        println!("  {name}");
    }

    if yes {
        return Ok(());
    }
    if !config::is_interactive() {
        bail!(
            "refusing to add {} package(s) unconfirmed; pass --yes",
            packages.len()
        );
    }

    let proceed = Confirm::new()
        .with_prompt("Add these packages?")
        .default(true)
        .interact()
        .context("failed to read confirmation")?;
    if !proceed {
        bail!("cancelled");
    }
    Ok(())
}

/// What one `pkg add` sends.
enum AddRequest {
    /// A patch belongs to one package's sources, and the bulk endpoint has
    /// nowhere to put it, so a patched add stays a single-package add.
    One(AddPackageRequest),
    /// One request for the whole list, so the server resolves every AUR name
    /// in one AUR request rather than one per package.
    Many(AddPackagesRequest),
}

pub(crate) async fn add_package_command(
    client: &AurCacheClient,
    format: OutputFormat,
    args: AddPackageArgs,
) -> Result<()> {
    let packages = if args.from_installed {
        let installed = pacman::installed_foreign_packages()?;
        if installed.is_empty() {
            bail!("`pacman -Qm` reported no foreign packages, so there is nothing to add");
        }
        let packages = pacman::merge_unique(args.packages, installed);
        confirm_bulk_add(&packages, args.yes)?;
        packages
    } else {
        args.packages
    };

    let sources = packages
        .into_iter()
        .map(|package| package_source(package, args.git_ref.as_deref(), &args.subfolder))
        .collect::<Result<Vec<_>>>()?;
    let platforms = some_vec(args.platforms);
    let build_flags = some_vec(args.build_flags);
    let request = if args.patches.is_empty() {
        AddRequest::Many(AddPackagesRequest {
            platforms,
            build_flags,
            sources,
        })
    } else {
        let [source] = <[SourceData; 1]>::try_from(sources)
            .map_err(|_| anyhow!("--patch can only be used when adding a single package"))?;
        AddRequest::One(AddPackageRequest {
            platforms,
            build_flags,
            source,
            patched_files: read_patch_files(&args.patches)?,
        })
    };

    // Before the request. An add fans out into a dependency tree whose build
    // numbers nobody can predict, so what makes this trigger's work
    // identifiable is not knowing the builds in advance but knowing which ones
    // were already there.
    let scope = args.wait.snapshot(client).await?;
    match request {
        AddRequest::One(request) => {
            client.add_package(&request).await?;
            print_done_message(format, "package add request complete");
        }
        AddRequest::Many(request) => {
            let accepted = client.add_packages(&request).await?;
            // Waited out before following builds: the job creates the
            // packages and enqueues the leaves as it goes, so before it ends
            // the delta is a half-built picture of what the add will produce.
            follow_bulk_add(client, format, accepted).await?;
        }
    }
    if let Some(scope) = &scope {
        report_queued(client, format, scope).await?;
    }
    follow_trigger(client, format, &args.wait, scope).await
}

/// One entry of `pkg add`: a git repository if it looks like one, otherwise
/// an AUR package name.
fn package_source(package: String, git_ref: Option<&str>, subfolder: &str) -> Result<SourceData> {
    if !looks_like_git_url(&package) {
        return Ok(SourceData::Aur { name: package });
    }
    let git_ref = git_ref.context("--ref is required when adding a git repository URL")?;
    Ok(SourceData::Git {
        spec: GitSourceSpec {
            url: package,
            r#ref: git_ref.to_string(),
            subfolder: subfolder.to_string(),
        },
    })
}

/// Say what a trigger queued, before following it.
///
/// The count is the first sign that dependency resolution went wrong -- an
/// implausible number of builds shows it long before any of them fails -- so it
/// is reported before the waiting starts rather than after.
pub(crate) async fn report_queued(
    client: &AurCacheClient,
    format: OutputFormat,
    scope: &WatchScope,
) -> Result<()> {
    let builds = list_scoped_builds(client, None).await?;
    let queued: Vec<_> = builds.iter().filter(|b| scope.includes(b)).collect();
    let packages: HashSet<&str> = queued.iter().map(|b| b.pkg_name.as_str()).collect();
    Progress(format).line(&format!(
        "queued {} build(s) across {} package(s)",
        queued.len(),
        packages.len()
    ));
    Ok(())
}

/// Report a bulk add until it finishes. A non-zero exit for failures is what
/// makes it usable from a script.
pub(crate) async fn follow_bulk_add(
    client: &AurCacheClient,
    format: OutputFormat,
    accepted: BulkAddAccepted,
) -> Result<()> {
    if format == OutputFormat::Text {
        println!(
            "adding {} package(s), job {}",
            accepted.total, accepted.job_id
        );
    }
    let run: BulkAddProgress = follow_job(
        format,
        |seen| client.bulk_add_progress(accepted.job_id, seen),
        print_bulk_add_entry,
    )
    .await?;
    if format == OutputFormat::Text {
        println!("done: {}, of {}", bulk_add_summary(&run), run.total);
    }
    bulk_add_result(&run)
}

pub(crate) fn print_bulk_add_entry(entry: &BulkAddEntry) {
    match &entry.outcome {
        BulkAddOutcome::Added => println!("  added    {}", entry.name),
        BulkAddOutcome::Existed => println!("  present  {}", entry.name),
        BulkAddOutcome::Failed { error } => println!("  failed   {}: {error}", entry.name),
    }
}

/// `3 added, 1 already present, 0 failed`.
///
/// Counted from the entries rather than read off the job, which folds "added"
/// and "already present" into one number: a restore wants to know how much of
/// it was already there, and reporting an untouched package as added is a
/// small lie the summary does not need to tell.
pub(crate) fn bulk_add_summary(run: &BulkAddProgress) -> String {
    let count = |wanted: fn(&BulkAddOutcome) -> bool| {
        run.entries.iter().filter(|e| wanted(&e.outcome)).count()
    };
    let added = count(|o| matches!(o, BulkAddOutcome::Added));
    let present = count(|o| matches!(o, BulkAddOutcome::Existed));
    let mut parts = vec![format!("{added} added")];
    if present > 0 {
        parts.push(format!("{present} already present"));
    }
    parts.push(format!("{} failed", run.failed));
    parts.join(", ")
}

/// A run with failures exits non-zero, naming how many, so a bulk add that only
/// partly worked is not mistaken for a clean one by whatever called it.
pub(crate) fn bulk_add_result(progress: &BulkAddProgress) -> Result<()> {
    if progress.failed > 0 {
        bail!("{} package(s) could not be added", progress.failed);
    }
    Ok(())
}

/// Reads the local files referenced by `--patch` arguments into a
/// path -> content map suitable for [`AddPackageRequest::patched_files`].
pub(crate) fn read_patch_files(patches: &[PatchArg]) -> Result<Option<BTreeMap<String, String>>> {
    if patches.is_empty() {
        return Ok(None);
    }
    let mut files = BTreeMap::new();
    for PatchArg {
        source_path,
        local_file,
    } in patches
    {
        let content = std::fs::read_to_string(local_file)
            .with_context(|| format!("failed to read patch file `{local_file}`"))?;
        if files.insert(source_path.clone(), content).is_some() {
            bail!("--patch specified for `{source_path}` more than once");
        }
    }
    Ok(Some(files))
}

pub(crate) async fn update_package_command(
    client: &AurCacheClient,
    format: OutputFormat,
    args: UpdatePackageArgs,
) -> Result<()> {
    // Before the request, so a build that is queued, runs and fails while we
    // are still reading the response is still ours to report.
    let scope = args.wait.snapshot(client).await?;
    let queued = client
        .update_package(&args.pkgbase, &UpdatePackage { force: args.force })
        .await?;
    render(format, &queued, |numbers| {
        print_queued_builds(&args.pkgbase, numbers);
    })?;
    let scope = scope.map(|scope| scope.triggered(&args.pkgbase, queued.iter().copied()));
    follow_trigger(client, format, &args.wait, scope).await
}

/// The numbers come back bare, so they are printed against the package they
/// belong to — `hello/4`, the same reference every other build command takes.
pub(crate) fn print_queued_builds(pkgbase: &str, numbers: &[i32]) {
    if numbers.is_empty() {
        println!("no builds were queued");
        return;
    }
    println!(
        "queued builds: {}",
        numbers
            .iter()
            .map(|n| format!("{pkgbase}/{n}"))
            .collect::<Vec<_>>()
            .join(", ")
    );
}

pub(crate) async fn patch_package_command(
    client: &AurCacheClient,
    format: OutputFormat,
    args: PatchPackageArgs,
) -> Result<()> {
    let has_metadata = !args.platforms.is_empty() || !args.build_flags.is_empty();
    let has_patches = !args.patches.is_empty();
    if !has_metadata && !has_patches {
        bail!("no changes specified");
    }

    if has_metadata {
        client
            .patch_package(&args.pkgbase, &patch_request(&args))
            .await?;
    }

    if has_patches {
        // `read_patch_files` validates duplicates and reads the local files.
        let files = read_patch_files(&args.patches)?.expect("non-empty patches produce a map");
        for (path, content) in files {
            client
                .put_source_file(&args.pkgbase, &path, &content)
                .await?;
        }
    }

    print_done_message(format, "package updated");
    Ok(())
}

/// The metadata half of a `pkg patch`: the platforms and build flags given.
pub(crate) fn patch_request(args: &PatchPackageArgs) -> PackagePatch {
    PackagePatch {
        build_flags: some_vec(args.build_flags.clone()),
        platforms: some_vec(args.platforms.clone()),
        ..PackagePatch::default()
    }
}

pub(crate) async fn run_dependency_command(
    client: &AurCacheClient,
    format: OutputFormat,
    command: DependencyCommand,
) -> Result<()> {
    match command {
        DependencyCommand::Options {
            pkgbase,
            dependency,
        } => {
            let options = client.dependency_options(&pkgbase, &dependency).await?;
            render(format, &options, print_dependency_options)
        }
        DependencyCommand::Replace {
            pkgbase,
            dependency,
            replacement,
        } => {
            client
                .replace_dependency(&pkgbase, &dependency, Some(&replacement))
                .await?;
            print_done_message(format, &format!("{pkgbase} now depends on {replacement}"));
            Ok(())
        }
        DependencyCommand::Drop {
            pkgbase,
            dependency,
        } => {
            client
                .replace_dependency(&pkgbase, &dependency, None)
                .await?;
            print_done_message(
                format,
                &format!("{pkgbase} no longer depends on {dependency}"),
            );
            Ok(())
        }
    }
}

pub(crate) fn print_dependency_options(options: &DependencyOptions) {
    println!("dependent: {}", options.dependent);
    println!("current: {}", options.current);
    println!(
        "declared as: {}",
        if options.declared_names.is_empty() {
            "nothing (stale edge)".to_string()
        } else {
            options.declared_names.join(", ")
        }
    );
    println!(
        "constraint: {}",
        option_text(Some(options.version_constraint.as_str()).filter(|c| !c.is_empty()))
    );
    if !options.official.is_empty() {
        println!(
            "official: {} -- `pkg dep drop` removes this dependency",
            options.official.join(", ")
        );
    }
    println!();

    if let Some(error) = &options.aur_error {
        println!("warning: the AUR could not be searched ({error});");
        println!("         only packages already tracked are listed below.");
        println!();
    }
    if options.candidates.is_empty() {
        println!("no replacement found");
        return;
    }
    let rows = options
        .candidates
        .iter()
        .map(|candidate| {
            vec![
                candidate.pkgbase.clone(),
                match candidate.source {
                    CandidateSource::Tracked => "tracked".to_string(),
                    CandidateSource::Aur => "aur".to_string(),
                },
                option_text(candidate.version.as_deref()),
                match candidate.verdict {
                    ReplacementVerdict::Satisfied => "yes",
                    ReplacementVerdict::Unknown => "unknown",
                    ReplacementVerdict::Unsatisfied => "no",
                }
                .to_string(),
            ]
        })
        .collect::<Vec<_>>();
    print_table(&["name", "source", "version", "satisfies"], &rows);
}

/// What `pkg rm` did, package by package.
#[derive(Debug, Default, Serialize)]
pub(crate) struct Removal {
    removed: Vec<String>,
    failed: Vec<FailedRemoval>,
}

#[derive(Debug, Serialize)]
pub(crate) struct FailedRemoval {
    package: String,
    error: String,
}

/// Removing several packages is one request per package, so one failure must
/// not hide the rest: every name is attempted, and what failed is reported
/// together at the end.
pub(crate) async fn remove_packages_command(
    client: &AurCacheClient,
    format: OutputFormat,
    pkgbases: &[String],
) -> Result<()> {
    let mut removal = Removal::default();
    for pkgbase in pkgbases {
        match client.delete_package(pkgbase).await {
            Ok(()) => removal.removed.push(pkgbase.clone()),
            Err(error) => removal.failed.push(FailedRemoval {
                package: pkgbase.clone(),
                error: format!("{error:#}"),
            }),
        }
    }

    render(format, &removal, |removal| {
        for pkgbase in &removal.removed {
            println!("removed {pkgbase}");
        }
    })?;

    if !removal.failed.is_empty() {
        let details = removal
            .failed
            .iter()
            .map(|failed| format!("  {}: {}", failed.package, failed.error))
            .collect::<Vec<_>>()
            .join("\n");
        bail!(
            "failed to remove {} of {} package(s):\n{details}",
            removal.failed.len(),
            pkgbases.len()
        );
    }
    Ok(())
}

pub(crate) fn some_vec<T>(values: Vec<T>) -> Option<Vec<T>> {
    (!values.is_empty()).then_some(values)
}

pub(crate) fn print_package_list(packages: &[SimplePackage]) {
    let rows = packages
        .iter()
        .map(|package| {
            vec![
                package.name.clone(),
                package.status.label().to_string(),
                bool_label(package.directly_requested).to_string(),
                bool_label(package.out_of_date).to_string(),
                option_text(package.built_version.as_deref()),
                option_text(package.upstream_version.as_deref()),
            ]
        })
        .collect::<Vec<_>>();
    print_table(
        &[
            "name",
            "status",
            "requested",
            "out_of_date",
            "built_version",
            "upstream_version",
        ],
        &rows,
    );
}

pub(crate) fn print_package(package: &ExtendedPackage) {
    print_package_summary(package);
    println!();
    print_dependency_section("dependencies", &package.dependencies);
    println!();
    print_dependency_section("dependents", &package.dependents);
}

pub(crate) fn print_package_summary(package: &ExtendedPackage) {
    println!("name: {}", package.name);
    println!("directly_requested: {}", package.directly_requested);
    println!("status: {}", package.status.label());
    println!("out_of_date: {}", package.out_of_date);
    println!(
        "built_version: {}",
        option_text(package.built_version.as_deref())
    );
    println!(
        "upstream_version: {}",
        option_text(package.upstream_version.as_deref())
    );
    println!("platforms: {}", join_or_dash(&package.selected_platforms));
    println!(
        "build_flags: {}",
        join_or_dash(package.selected_build_flags.as_deref().unwrap_or_default())
    );
    println!("source: {}", describe_source(&package.package_source));
    println!(
        "split_packages: {}",
        package
            .split_packages
            .as_ref()
            .map(|packages| join_or_dash(packages))
            .unwrap_or_else(|| "-".to_string())
    );
}

pub(crate) fn print_dependency_section(title: &str, dependencies: &[PackageDependency]) {
    println!("{title}:");
    if dependencies.is_empty() {
        println!("  -");
        return;
    }
    for dependency in dependencies {
        let state = if dependency.satisfied {
            String::new()
        } else {
            match dependency.built_version.as_deref() {
                Some(built) => format!(" BLOCKING: has {built}"),
                None => " BLOCKING: never built".to_string(),
            }
        };
        println!(
            "  - {} ({}){}",
            dependency.name, dependency.version_constraint, state
        );
    }
}

pub(crate) fn describe_source(source: &PackageSource) -> String {
    match source {
        PackageSource::Aur(aur) => format!("aur ({})", aur.aur_url),
        PackageSource::AurNotFound(_) => "aur (package metadata unavailable)".to_string(),
        PackageSource::Git(git) => {
            format!(
                "git (url={}, ref={}, subfolder={})",
                git.url, git.r#ref, git.subfolder
            )
        }
        PackageSource::Upload(_) => "upload".to_string(),
    }
}
