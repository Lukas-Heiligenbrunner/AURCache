//! `build-once`: build a local PKGBUILD directory in a chroot without any
//! server, for fast developer iteration. Reuses the same chroot machinery as
//! the polling path.

use anyhow::{Context, Result, bail};
use std::path::Path;

use aurcache_common::worker::BuildOutcome;
use aurcache_worker_core::{artifacts, report};

use crate::build;
use crate::config::Config;
use crate::devtools;
use crate::executor::Shared;

/// Build the PKGBUILD in `path` locally and report where the artifacts landed.
pub async fn build_once(cfg: &Config, path: &Path, flags: &[String]) -> Result<()> {
    let pkgdir = path
        .canonicalize()
        .with_context(|| format!("resolving {}", path.display()))?;
    if !pkgdir.join("PKGBUILD").exists() {
        bail!("no PKGBUILD found in {}", pkgdir.display());
    }
    tracing::info!("Building {} locally", pkgdir.display());

    // Seed the base chroot from the host's pacman.conf: a local one-shot build
    // is explicitly "build this the way this machine would", so the host's
    // repositories are the right ones. `makepkg.conf` is still left to the
    // chroot's own, matching what a served build gets.
    let pacman_conf = existing("/etc/pacman.conf")?;
    let shared = Shared::new(cfg);
    let chroots = &shared.chroots;
    chroots.open().await?;
    chroots
        .refresh(&pacman_conf, None)
        .await
        .context("preparing base chroot")?;
    let cache = shared.cache(cfg.tunables.cache).await;
    let pkgbase = pkgdir
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("local");
    let srcdest = cache.srcdest(pkgbase);

    // Through `with_lease` like the polling path: a spawn failure must give
    // the chroot back too, not just the happy path. The release happens
    // after the child has exited, never before. The chroot is held to the
    // same disk quota a served build gets; the packages land in `pkgdir`,
    // which is the developer's own directory.
    let status = chroots
        .with_lease(
            crate::chroots::ONE_SHOT_BUILD_ID,
            Some(cfg.tunables.build_disk_max),
            async |lease| {
                let argv = build::build_command(
                    lease.chroot_dir(),
                    lease.label(),
                    &cfg.bind_mounts,
                    flags,
                    &cfg.build_user,
                );
                tracing::info!("$ sudo {}", argv.join(" "));

                let mut cmd = devtools::command_in(&argv[0], &lease.tmpdir());
                cmd.args(&argv[1..]).current_dir(&pkgdir);
                // devtools binds `$SRCDEST` itself; unset, it falls back to the
                // PKGBUILD directory and downloads are not cached between runs.
                if let Some(dir) = srcdest.as_deref() {
                    cmd.env("SRCDEST", dir);
                }
                cmd.status().await.context("running build")
            },
        )
        .await?;

    let report = report::ended(None, std::time::Duration::ZERO, status.into());
    if report.outcome == BuildOutcome::Succeeded {
        let artifacts = artifacts::discover_artifacts(&pkgdir);
        tracing::info!("Build succeeded: {} artifact(s)", artifacts.len());
        for a in artifacts {
            println!("{}", a.display());
        }
        Ok(())
    } else {
        bail!(
            "build failed: {}",
            report.reason.as_deref().unwrap_or("unknown")
        );
    }
}

fn existing(path: &str) -> Result<std::path::PathBuf> {
    let p = std::path::PathBuf::from(path);
    if p.exists() {
        Ok(p)
    } else {
        bail!("{path} not found (run on an Arch host/image)");
    }
}
