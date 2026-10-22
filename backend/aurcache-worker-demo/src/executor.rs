//! The demo executor: synthetic packages, no build.
//!
//! Each claimed job waits a beat (so the queue UI reads as alive rather than
//! instant), writes one minimal-but-valid `.pkg.tar.zst` per expected package
//! name, and uploads them through the same staging endpoint a real worker
//! uses. Publish, `repo.db` updates and dependent fan-out then behave exactly
//! as in production.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use aurcache_common::worker::{CompleteReport, JobDescriptor};
use aurcache_worker_core::client::WorkerClient;
use aurcache_worker_core::config::env_opt;
use aurcache_worker_core::executor::Executor;
use aurcache_worker_core::{protocol, report};

/// How long a fake build appears to take, in seconds.
///
/// From `DEMO_BUILD_SECS` (default 3): long enough to watch a build move
/// through the queue UI, short enough that dependency fan-out stays lively.
fn fake_build_secs() -> u64 {
    env_opt("DEMO_BUILD_SECS")
        .and_then(|v| v.parse().ok())
        .unwrap_or(3)
}

/// Builds nothing, convincingly and quickly.
pub struct DemoExecutor;

impl DemoExecutor {
    /// A demo executor holds no machine state: every job is independent.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Default for DemoExecutor {
    fn default() -> Self {
        Self::new()
    }
}

impl Executor for DemoExecutor {
    const KIND: &'static str = "demo";

    async fn run_job(
        &self,
        client: Arc<WorkerClient>,
        job: JobDescriptor,
        _mirrorlist: Option<String>,
        cancel: Arc<AtomicBool>,
    ) -> CompleteReport {
        let build_id = job.build_id;
        protocol::log(
            &client,
            build_id,
            "[demo] synthetic build: nothing is compiled, archives are near-empty\n",
        )
        .await;

        let names = job.expected_packages.clone();

        // The visible beat, in slices so a cancel lands promptly rather
        // than after the whole wait.
        let deadline = std::time::Instant::now() + Duration::from_secs(fake_build_secs());
        while std::time::Instant::now() < deadline {
            if cancel.load(Ordering::Relaxed) || protocol::cancel_requested(&client, build_id).await
            {
                return report::canceled();
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        if cancel.load(Ordering::Relaxed) {
            return report::canceled();
        }

        match fake_and_upload(&client, &job, &names).await {
            Ok(()) => report::success(),
            Err(e) => report::interrupted(format!("demo packaging failed: {e:#}")),
        }
    }

    fn describe_self(&self) -> String {
        "demo (synthetic near-empty packages; nothing is compiled)".to_string()
    }
}

/// Write one fake archive per package name into a staging directory and
/// upload the directory through the shared artifact path.
async fn fake_and_upload(
    client: &WorkerClient,
    job: &JobDescriptor,
    names: &[String],
) -> Result<()> {
    let dir = tempfile::tempdir().context("demo staging directory")?;
    for name in names {
        let bytes = fake_package_bytes(name, &job.pkgbase, &job.expected_version, &job.arch)?;
        let filename = format!("{name}-{}-{}.pkg.tar.zst", job.expected_version, job.arch);
        std::fs::write(dir.path().join(&filename), &bytes)
            .with_context(|| format!("writing {filename}"))?;
    }
    protocol::upload_artifacts(client, job.build_id, dir.path()).await
}

/// A minimal `.pkg.tar.zst`: a `.PKGINFO` (the only entry publish reads)
/// plus one small payload file so the archive is not literally empty.
///
/// `version` is the full `pkgver-pkgrel` string, used verbatim in both the
/// filename the caller builds and `.PKGINFO`'s `pkgver`, exactly as makepkg
/// would write them.
pub fn fake_package_bytes(
    pkgname: &str,
    pkgbase: &str,
    version: &str,
    arch: &str,
) -> Result<Vec<u8>> {
    let builddate = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let readme = format!(
        "This is a synthetic demo package for {pkgname}.\n\
         It was not compiled; it exists so the demo instance can show \
         dependency resolution and publishing without building anything.\n"
    );
    let pkginfo = format!(
        "pkgname = {pkgname}\n\
         pkgbase = {pkgbase}\n\
         pkgver = {version}\n\
         pkgdesc = Synthetic demo package (no real build ran)\n\
         url = demo\n\
         arch = {arch}\n\
         builddate = {builddate}\n\
         packager = AURCache demo worker\n\
         size = {}\n",
        readme.len()
    );

    let mut tar_buf = Vec::new();
    {
        let mut tar = tar::Builder::new(&mut tar_buf);
        append_bytes(&mut tar, ".PKGINFO", pkginfo.as_bytes(), 0o644, builddate)?;
        append_bytes(
            &mut tar,
            &format!("usr/share/doc/{pkgname}/README.demo"),
            readme.as_bytes(),
            0o644,
            builddate,
        )?;
        tar.finish().context("finishing demo tar")?;
    }
    zstd::stream::encode_all(tar_buf.as_slice(), 3).context("compressing demo package")
}

/// Append one in-memory file to a tar builder, the way `artifacts` tests do.
fn append_bytes(
    tar: &mut tar::Builder<&mut Vec<u8>>,
    path: &str,
    data: &[u8],
    mode: u32,
    mtime: u64,
) -> Result<()> {
    let mut header = tar::Header::new_gnu();
    header.set_size(data.len() as u64);
    header.set_mode(mode);
    header.set_mtime(mtime);
    header.set_cksum();
    tar.append_data(&mut header, path, data)
        .with_context(|| format!("adding {path} to demo package"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Write `bytes` to a temp file named `filename` and run the real
    /// server-side read over it: this is the exact function publish calls,
    /// so passing here means the demo archive publishes.
    fn describe(filename: &str, bytes: &[u8]) -> pacman_repo_utils::PackageEntry {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(filename);
        std::fs::write(&path, bytes).unwrap();
        pacman_repo_utils::describe_package(&path).unwrap()
    }

    #[test]
    fn single_package_round_trips_through_describe() {
        let bytes = fake_package_bytes("hello", "hello", "2.12.1-1", "x86_64").unwrap();
        let entry = describe("hello-2.12.1-1-x86_64.pkg.tar.zst", &bytes);

        assert_eq!(entry.filename, "hello-2.12.1-1-x86_64.pkg.tar.zst");
        assert_eq!(entry.dir_name, "hello-2.12.1-1");
    }

    #[test]
    fn each_split_name_produces_its_own_valid_archive() {
        for name in ["split-base", "split-base-docs"] {
            let bytes = fake_package_bytes(name, "split-base", "1.0-1", "x86_64").unwrap();
            let entry = describe(&format!("{name}-1.0-1-x86_64.pkg.tar.zst"), &bytes);

            assert_eq!(entry.dir_name, format!("{name}-1.0-1"));
            assert!(
                entry.filename.starts_with(name),
                "unexpected filename {}",
                entry.filename
            );
        }
    }

    #[test]
    fn archive_lists_its_payload_but_not_dotfiles() {
        let bytes = fake_package_bytes("hello", "hello", "1.0-1", "aarch64").unwrap();
        assert!(!bytes.is_empty(), "a demo archive must not be empty");
        // Feed the tar (not the zst) view: decompress and list entry paths.
        let tar = zstd::stream::decode_all(bytes.as_slice()).unwrap();
        let mut archive = tar::Archive::new(tar.as_slice());
        let mut paths: Vec<String> = archive
            .entries()
            .unwrap()
            .map(|e| e.unwrap().path().unwrap().display().to_string())
            .collect();
        paths.sort_unstable();
        assert_eq!(
            paths,
            vec![
                ".PKGINFO".to_string(),
                "usr/share/doc/hello/README.demo".to_string()
            ]
        );
    }
}
