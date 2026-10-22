//! `packages`: adding, updating, patching and removing packages.

use crate::builds::{WatchScope, follow_builds, list_watched_builds, snapshot_builds};
use crate::cli::{
    AddPackageArgs, BuildRef, DependencyCommand, ListPackagesArgs, OutputFormat, PackagesCommand,
    PatchArg, PatchPackageArgs, UpdatePackageArgs, WaitOpts,
};
use crate::output::{
    PROGRESS_POLL_INTERVAL_SECS, Progress, bool_label, join_or_dash, option_text,
    print_done_message, print_json, print_table, render,
};
use crate::{config, pacman};
use anyhow::{Context, Result, anyhow, bail};
use aurcache_client::{
    AddPackageRequest, AddPackagesRequest, AurCacheClient, Build, BulkAddAccepted, BulkAddOutcome,
    BulkAddProgress, CandidateSource, DependencyOptions, ExtendedPackage, GitSourceSpec,
    PackageDependency, PackagePatch, PackageSource, ReplacementVerdict, SimplePackage, SourceData,
    UpdatePackage, looks_like_git_url,
};
use dialoguer::Confirm;
use serde_json::json;
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

    if !args.patches.is_empty() && packages.len() > 1 {
        bail!("--patch can only be used when adding a single package");
    }
    let git_entries = packages.iter().filter(|p| looks_like_git_url(p)).count();
    if git_entries > 0 && args.git_ref.is_none() {
        bail!("--ref is required when adding a git repository URL");
    }
    let git_ref = args.git_ref;

    let patched_files = read_patch_files(&args.patches)?;
    // Before the request. An add fans out into a dependency tree whose build
    // numbers nobody can predict, so what makes this trigger's work
    // identifiable is not knowing the builds in advance but knowing which ones
    // were already there.
    let scope = if args.wait.wait {
        Some(snapshot_builds(client, None).await?)
    } else {
        None
    };
    let mut sources = Vec::new();
    for package in packages {
        sources.push(if looks_like_git_url(&package) {
            SourceData::Git {
                spec: GitSourceSpec {
                    url: package,
                    r#ref: git_ref.clone().ok_or_else(|| {
                        anyhow!("--ref is required when adding a git repository URL")
                    })?,
                    subfolder: args.subfolder.clone(),
                },
            }
        } else {
            SourceData::Aur { name: package }
        });
    }

    // A patch belongs to one package's sources, and the bulk endpoint has
    // nowhere to put it, so a patched add stays a single-package add. The
    // argument parser already refuses `--patch` with more than one package.
    if patched_files.is_some() {
        let source = sources
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("no package given"))?;
        client
            .add_package(&AddPackageRequest {
                platforms: some_vec(args.platforms.clone()),
                build_flags: some_vec(args.build_flags.clone()),
                source,
                patched_files,
            })
            .await?;
        if format == OutputFormat::Text {
            println!("package add request complete");
        }
        return match scope {
            Some(scope) => wait_for_queued(client, format, &args.wait, scope).await,
            None => Ok(()),
        };
    }

    // One request for the whole list. Adding them one at a time made the server
    // resolve each AUR name to its pkgbase on its own -- an AUR request per
    // package, before any source was fetched.
    let accepted = client
        .add_packages(&AddPackagesRequest {
            platforms: some_vec(args.platforms.clone()),
            build_flags: some_vec(args.build_flags.clone()),
            sources,
        })
        .await?;

    follow_bulk_add(client, format, accepted).await?;
    // Only after the add job has finished: it creates the packages and enqueues
    // the leaves as it goes, so before that the delta is a half-built picture of
    // what the add will produce.
    match scope {
        Some(scope) => wait_for_queued(client, format, &args.wait, scope).await,
        None => Ok(()),
    }
}

/// Say what a trigger queued, then follow it to the end.
///
/// The count is the first sign that dependency resolution went wrong -- an
/// implausible number of builds shows it long before any of them fails -- so it
/// is reported before the waiting starts rather than after.
pub(crate) async fn wait_for_queued(
    client: &AurCacheClient,
    format: OutputFormat,
    wait: &WaitOpts,
    scope: WatchScope,
) -> Result<()> {
    let progress = Progress(format);
    let queued: Vec<Build> = list_watched_builds(client, None)
        .await?
        .into_iter()
        .filter(|b| scope.includes(b))
        .collect();
    let packages: HashSet<&str> = queued.iter().map(|b| b.pkg_name.as_str()).collect();
    progress.line(&format!(
        "queued {} build(s) across {} package(s)",
        queued.len(),
        packages.len()
    ));
    follow_builds(client, progress, &wait.as_watch_args(), scope).await
}

/// Report a bulk add until it finishes.
///
/// The server does not need us here -- the job runs whether or not anything
/// watches -- so this is reporting, not driving. A non-zero exit for failures
/// is what makes it usable from a script.
pub(crate) async fn follow_bulk_add(
    client: &AurCacheClient,
    format: OutputFormat,
    accepted: BulkAddAccepted,
) -> Result<()> {
    if format == OutputFormat::Json {
        // Machine output waits for the end and prints the whole run at once:
        // a stream of partial states is harder to consume than one final
        // document, and the run is what the caller asked about. Polled
        // incrementally and reassembled here — refetching the whole history
        // per tick would be O(n²) transfer over a long run.
        let mut full = client.bulk_add_progress(accepted.job_id, 0).await?;
        let mut seen = full.entries.len();
        while !full.finished {
            tokio::time::sleep(std::time::Duration::from_secs(PROGRESS_POLL_INTERVAL_SECS)).await;
            let next = client.bulk_add_progress(accepted.job_id, seen).await?;
            seen += next.entries.len();
            full.entries.extend(next.entries);
            full.completed = next.completed;
            full.failed = next.failed;
            full.finished = next.finished;
        }
        println!("{}", serde_json::to_string_pretty(&full)?);
        return bulk_add_result(&full);
    }

    println!(
        "adding {} package(s), job {}",
        accepted.accepted, accepted.job_id
    );
    let mut seen = 0_usize;
    // Counted here rather than read off the job, which folds "added" and
    // "already present" into one number: a restore wants to know how much of it
    // was already there, and reporting an untouched package as added is a
    // small lie the summary does not need to tell.
    let (mut added, mut present) = (0_usize, 0_usize);
    loop {
        let progress = client.bulk_add_progress(accepted.job_id, seen).await?;
        for entry in &progress.entries {
            match &entry.outcome {
                BulkAddOutcome::Added => {
                    added += 1;
                    println!("  added    {}", entry.name);
                }
                BulkAddOutcome::Existed => {
                    present += 1;
                    println!("  present  {}", entry.name);
                }
                BulkAddOutcome::Failed { error } => {
                    println!("  failed   {}: {error}", entry.name);
                }
            }
        }
        seen += progress.entries.len();
        if progress.finished {
            let mut parts = vec![format!("{added} added")];
            if present > 0 {
                parts.push(format!("{present} already present"));
            }
            parts.push(format!("{} failed", progress.failed));
            println!("done: {}, of {}", parts.join(", "), progress.total);
            return bulk_add_result(&progress);
        }
        tokio::time::sleep(std::time::Duration::from_secs(PROGRESS_POLL_INTERVAL_SECS)).await;
    }
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
    let scope = if args.wait.wait {
        Some(snapshot_builds(client, None).await?)
    } else {
        None
    };
    let queued = client
        .update_package(&args.pkgbase, &UpdatePackage { force: args.force })
        .await?;
    render(format, &queued, |numbers| {
        print_queued_builds(&args.pkgbase, numbers);
    })?;
    let Some(scope) = scope else {
        return Ok(());
    };
    // The response names what it enqueued, which pins those builds into scope
    // whatever state they have reached by now. It omits a build left waiting on
    // a dependency, which is why this package's unfinished builds are adopted
    // too: an update that reused an existing row still has to wait for it.
    let scope = scope
        .with_explicit(queued.iter().map(|&number| BuildRef {
            pkgbase: args.pkgbase.clone(),
            number,
        }))
        .adopting(&args.pkgbase);
    follow_builds(client, Progress(format), &args.wait.as_watch_args(), scope).await
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

/// Removing several packages is one request per package, so one failure must
/// not hide the rest: every name is attempted, and what failed is reported
/// together at the end.
pub(crate) async fn remove_packages_command(
    client: &AurCacheClient,
    format: OutputFormat,
    pkgbases: &[String],
) -> Result<()> {
    let mut removed = Vec::new();
    let mut failed = Vec::new();
    for pkgbase in pkgbases {
        match client.delete_package(pkgbase).await {
            Ok(()) => removed.push(pkgbase.clone()),
            Err(error) => failed.push((pkgbase.clone(), format!("{error:#}"))),
        }
    }

    match format {
        OutputFormat::Json => print_json(&json!({
            "removed": removed,
            "failed": failed
                .iter()
                .map(|(pkgbase, error)| json!({ "package": pkgbase, "error": error }))
                .collect::<Vec<_>>(),
        }))?,
        OutputFormat::Text => {
            for pkgbase in &removed {
                println!("removed {pkgbase}");
            }
        }
    }

    if !failed.is_empty() {
        let details = failed
            .iter()
            .map(|(pkgbase, error)| format!("  {pkgbase}: {error}"))
            .collect::<Vec<_>>()
            .join("\n");
        bail!(
            "failed to remove {} of {} package(s):\n{details}",
            failed.len(),
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
