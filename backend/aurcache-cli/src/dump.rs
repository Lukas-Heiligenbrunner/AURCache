//! `dump` and `restore`: an instance's state to a file and back.

use crate::cli::{OutputFormat, RestoreArgs};
use crate::output::{follow_job, render};
use anyhow::{Context, Result, bail};
use aurcache_client::{
    AurCacheClient, RestoreEntry, RestoreOptions, RestoreOutcome, RestoreProgress,
};
use serde::Serialize;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Restore a dump, reporting each package as the server gets to it.
pub(crate) async fn restore_command(
    client: &AurCacheClient,
    format: OutputFormat,
    args: RestoreArgs,
) -> Result<()> {
    let bytes = std::fs::read(&args.archive)
        .with_context(|| format!("failed to read {}", args.archive.display()))?;

    let options = RestoreOptions {
        dry_run: args.dry_run,
        on_existing: args.on_existing.into(),
        clear: args.clear,
        secrets: args.secrets.into(),
    };
    let accepted = client.restore(bytes.into(), &options).await?;

    // A dry run has nothing to poll: it changed nothing, and what it would have
    // done is already in hand.
    let Some(job_id) = accepted.job_id else {
        render(format, &accepted, |accepted| {
            if args.clear {
                println!("would first remove every package, setting and worker here");
            }
            println!("would apply {} package(s):", accepted.total);
            accepted.preview.iter().for_each(print_restore_entry);
        })?;
        // A dry run that found something blocking exits non-zero, so
        // `restore --dry-run && restore` cannot walk into the failure it was
        // run to discover. Reporting the problem and then reporting success is
        // the one thing a preview must not do.
        let blocked = accepted
            .preview
            .iter()
            .filter(|e| matches!(e.outcome, RestoreOutcome::Failed { .. }))
            .count();
        return restore_result(i32::try_from(blocked).unwrap_or(i32::MAX));
    };

    if format == OutputFormat::Text {
        println!("restoring {} package(s), job {job_id}", accepted.total);
    }
    let run: RestoreProgress = follow_job(
        format,
        |seen| client.restore_progress(job_id, seen),
        print_restore_entry,
    )
    .await?;
    if format == OutputFormat::Text {
        println!(
            "done: {} applied, {} failed, of {}",
            run.completed, run.failed, run.total
        );
    }
    restore_result(run.failed)
}

/// One package of a restore, or of the preview of one.
pub(crate) fn print_restore_entry(entry: &RestoreEntry) {
    println!("  {:<10} {}", entry.outcome.label(), entry.pkgbase);
    if let RestoreOutcome::Failed { error } = &entry.outcome {
        println!("             {error}");
    }
}

/// A restore with failures exits non-zero, so a partly-applied dump is not
/// mistaken for a clean one by whatever called it.
pub(crate) fn restore_result(failed: i32) -> Result<()> {
    if failed > 0 {
        bail!("{failed} package(s) could not be restored");
    }
    Ok(())
}

/// Write the server's dump to a file.
///
/// A file rather than stdout by default: it is a `.tar.gz`, and a shell that
/// swallows it into a terminal is a worse default than one that has to be
/// redirected deliberately. `--output -` still writes to stdout for a caller
/// that wants to pipe it.
pub(crate) async fn dump_command(
    client: &AurCacheClient,
    format: OutputFormat,
    output: Option<PathBuf>,
    include_secrets: bool,
) -> Result<()> {
    let bytes = client.dump(include_secrets).await?;

    let path = output.unwrap_or_else(|| {
        PathBuf::from(format!(
            "aurcache-dump-{}.tar.gz",
            jiff::Zoned::now().strftime("%Y%m%d")
        ))
    });

    if path.as_os_str() == "-" {
        std::io::stdout()
            .write_all(&bytes)
            .context("failed to write dump to stdout")?;
        return Ok(());
    }

    // A dump with secrets is a credential. Written owner-only, and created that
    // way rather than chmod'ed afterwards -- between the two there is a moment
    // where the CA private key is world-readable.
    write_dump_file(&path, &bytes, include_secrets)
        .with_context(|| format!("failed to write dump to {}", path.display()))?;

    let written = WrittenDump {
        path: &path,
        bytes: bytes.len(),
    };
    render(format, &written, |written| {
        println!("wrote {} ({} bytes)", written.path.display(), written.bytes);
        if include_secrets {
            println!(
                "This file contains the CA private key. Anyone holding it can \
                 act as a build worker for this server."
            );
        }
    })
}

/// Where a dump went, and how big it is.
#[derive(Serialize)]
pub(crate) struct WrittenDump<'a> {
    path: &'a Path,
    bytes: usize,
}

/// Write the archive, owner-only when it carries secrets.
pub(crate) fn write_dump_file(path: &Path, bytes: &[u8], private: bool) -> Result<()> {
    if private {
        aurcache_common::fs::write_private(path, bytes)?;
    } else {
        std::fs::write(path, bytes)?;
    }
    Ok(())
}
