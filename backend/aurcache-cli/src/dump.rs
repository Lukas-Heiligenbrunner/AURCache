//! `dump` and `restore`: an instance's state to a file and back.

use crate::cli::{OnExisting, OutputFormat, Secrets};
use crate::output::PROGRESS_POLL_INTERVAL_SECS;
use anyhow::{Context, Result, bail};
use aurcache_client::{AurCacheClient, RestoreOutcome};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Restore a dump, reporting each package as the server gets to it.
pub(crate) async fn restore_command(
    client: &AurCacheClient,
    format: OutputFormat,
    archive: &Path,
    dry_run: bool,
    on_existing: OnExisting,
    clear: bool,
    secrets: Secrets,
) -> Result<()> {
    let bytes =
        std::fs::read(archive).with_context(|| format!("failed to read {}", archive.display()))?;

    let accepted = client
        .restore(
            bytes.into(),
            dry_run,
            on_existing.into(),
            clear,
            secrets.into(),
        )
        .await?;

    // A dry run has nothing to poll: it changed nothing, and what it would have
    // done is already in hand.
    let Some(job_id) = accepted.job_id else {
        if format == OutputFormat::Json {
            println!("{}", serde_json::to_string_pretty(&accepted)?);
        } else {
            if clear {
                println!("would first remove every package, setting and worker here");
            }
            println!("would apply {} package(s):", accepted.total);
            for entry in &accepted.preview {
                println!("  {:<10} {}", entry.outcome.label(), entry.pkgbase);
                if let RestoreOutcome::Failed { error } = &entry.outcome {
                    println!("             {error}");
                }
            }
        }
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

    if format == OutputFormat::Json {
        // Incremental like the bulk-add follower above: the final document
        // still shows the whole run, without re-transferring it per tick.
        let mut full = client.restore_progress(job_id, 0).await?;
        let mut seen = full.entries.len();
        while !full.finished {
            tokio::time::sleep(std::time::Duration::from_secs(PROGRESS_POLL_INTERVAL_SECS)).await;
            let next = client.restore_progress(job_id, seen).await?;
            seen += next.entries.len();
            full.entries.extend(next.entries);
            full.completed = next.completed;
            full.failed = next.failed;
            full.finished = next.finished;
        }
        println!("{}", serde_json::to_string_pretty(&full)?);
        return restore_result(full.failed);
    }

    println!("restoring {} package(s), job {job_id}", accepted.total);
    let mut seen = 0_usize;
    loop {
        let progress = client.restore_progress(job_id, seen).await?;
        for entry in &progress.entries {
            println!("  {:<10} {}", entry.outcome.label(), entry.pkgbase);
            if let RestoreOutcome::Failed { error } = &entry.outcome {
                println!("             {error}");
            }
        }
        seen += progress.entries.len();
        if progress.finished {
            println!(
                "done: {} applied, {} failed, of {}",
                progress.completed, progress.failed, progress.total
            );
            return restore_result(progress.failed);
        }
        tokio::time::sleep(std::time::Duration::from_secs(PROGRESS_POLL_INTERVAL_SECS)).await;
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

    match format {
        OutputFormat::Json => println!(
            "{}",
            serde_json::json!({ "path": path.display().to_string(), "bytes": bytes.len() })
        ),
        OutputFormat::Text => {
            println!("wrote {} ({} bytes)", path.display(), bytes.len());
            if include_secrets {
                println!(
                    "This file contains the CA private key. Anyone holding it can \
                     act as a build worker for this server."
                );
            }
        }
    }
    Ok(())
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
