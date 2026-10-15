//! End-to-end execution of a single claimed job: download the (server-patched)
//! source, prepare a per-package chroot copy, import trusted keys, build in a
//! cgroup of its own while streaming logs and honoring cancellation, then
//! upload artifacts. Returns a terminal [`CompleteReport`]; never panics past
//! the caller's wrapper.

use anyhow::{Context, Result};
use aurcache_common::worker::{BuildOutcome, CompleteReport, JobDescriptor};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use aurcache_worker_core::client::WorkerClient;
use aurcache_worker_core::protocol::{Stop, StopWatch, log, report_warning, upload_artifacts};
use aurcache_worker_core::{artifacts, report};

use crate::build::{self, BindMount};
use crate::cache::Cache;
use crate::cgroup::Hierarchy;
use crate::chroot;
use crate::chroots::Lease;
use crate::config::Config;
use crate::credentials;
use crate::executor::Shared;
use crate::repo_db;

/// Await a blocking cache task, logging the panic instead of swallowing it.
///
/// Every cache task below is best-effort, so a dead one must not fail the
/// job — but `unwrap_or`/`let _` on the join also buried *which* task died,
/// leaving a panic with no trace. `Err` here is a panic (or a runtime
/// shutdown), never a cache miss: the cache fns report those themselves.
async fn join_cache_task<T>(handle: tokio::task::JoinHandle<T>, task: &'static str) -> Option<T> {
    match handle.await {
        Ok(value) => Some(value),
        Err(e) => {
            tracing::warn!("background cache task '{task}' died: {e}");
            None
        }
    }
}

/// Run one job to completion and return its terminal report.
///
/// The per-job workspace is removed here rather than at the end of the happy
/// path, so every early exit — source download, chroot prep, build failure,
/// artifact upload — drops its extracted sources and partial artifacts too.
/// Build ids are per-attempt, so a leaked workdir would never be reclaimed by a
/// later retry and `WORKER_DATA_DIR/work/` would grow without bound.
pub async fn run_job(
    cfg: &Config,
    cgroups: Option<&Hierarchy>,
    client: &Arc<WorkerClient>,
    job: JobDescriptor,
    mirrorlist: Option<&str>,
    cancel: Arc<AtomicBool>,
    shared: Arc<Shared>,
) -> CompleteReport {
    let workdir = workdir(cfg, job.build_id);
    let report = match run_job_inner(cfg, cgroups, client, &job, mirrorlist, &cancel, &shared).await
    {
        Ok(report) => report,
        Err(e) => {
            let report = report::from_error(&e);
            let reason = report.reason.as_deref().unwrap_or_default();
            log(client, job.build_id, &format!("\n[worker] {reason}\n")).await;
            report
        }
    };

    // Keep the shared caches and base chroot; only this job's tree goes.
    if let Err(e) = tokio::fs::remove_dir_all(&workdir).await
        && e.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!("failed to clean workdir {}: {e}", workdir.display());
    }
    report
}

/// The per-job workspace of build `build_id`.
fn workdir(cfg: &Config, build_id: i32) -> PathBuf {
    cfg.core.data_dir.join("work").join(build_id.to_string())
}

async fn run_job_inner(
    cfg: &Config,
    cgroups: Option<&Hierarchy>,
    client: &Arc<WorkerClient>,
    job: &JobDescriptor,
    mirrorlist: Option<&str>,
    cancel: &AtomicBool,
    shared: &Shared,
) -> Result<CompleteReport> {
    let mut prepared = prepare_job(cfg, client, job, mirrorlist, shared).await?;
    if cancel.load(Ordering::SeqCst) {
        return Ok(report::canceled());
    }
    let lease = shared
        .chroots
        .acquire(job.build_id, Some(cfg.build_disk_max))
        .await?;
    log(client, job.build_id, "[worker] starting build\n").await;
    let context = WorkerContext {
        cfg,
        cgroups,
        dropin: &prepared.dropin,
        lease: &lease,
        chroots: &shared.chroots,
    };
    let outcome = execute_prepared_job(&context, client, job, cancel, &prepared).await;
    let outcome =
        settle_build_lease(cfg, client, job, shared, &prepared.cache, lease, outcome).await;
    let succeeded = outcome
        .as_ref()
        .is_ok_and(|report| report.outcome == BuildOutcome::Succeeded);
    discard_failed_tree(client, job, &prepared.cache, succeeded).await;
    finish_package_cache(client, job, &prepared.cache, &prepared.cleanup.label).await;
    prepared.cleanup.active = false;
    outcome
}

/// Resources prepared for one job, held until reporting and cache cleanup end.
struct PreparedJob {
    cache: Cache,
    dropin: PathBuf,
    binds: Vec<BindMount>,
    cleanup: JobCacheCleanup,
}

/// Clean private cache replicas even if preparation or artifact upload fails.
struct JobCacheCleanup {
    cache: Cache,
    label: String,
    active: bool,
}
impl Drop for JobCacheCleanup {
    fn drop(&mut self) {
        if self.active
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            let cache = self.cache.clone();
            let label = self.label.clone();
            runtime.spawn_blocking(move || {
                cache.wipe_pacman_pkg_job(&label);
                cache.wipe_gnupg_job(&label);
            });
        }
    }
}

/// Stage the job's configs and caches and prepare its bind mounts.
async fn prepare_job(
    cfg: &Config,
    client: &Arc<WorkerClient>,
    job: &JobDescriptor,
    mirrorlist: Option<&str>,
    shared: &Shared,
) -> Result<PreparedJob> {
    let build_id = job.build_id;
    let workdir = &workdir(cfg, build_id);
    let cache = Cache::new(
        &cfg.cache_dir,
        cfg.cache_max_size,
        cfg.cache_ttl,
        cfg.pkgcache_max_size,
        cfg.pkgcache_ttl,
    )
    .with_volumes(shared.chroots.cache_volumes().await);

    let job_label = format!("job-{build_id}");
    let cleanup = JobCacheCleanup {
        cache: cache.clone(),
        label: job_label.clone(),
        active: true,
    };

    // Opportunistic cache GC (never blocks the build). Pin every pkgbase that is
    // currently building — not just this job's — so a concurrent sibling's
    // in-progress SRCDEST is never wiped out from under it.
    //
    // The scan recurses the whole srcdest tree (up to `cache_max_size`) with
    // blocking `read_dir`/`metadata`, so it runs on the blocking pool: with
    // `concurrency` jobs starting at once it would otherwise tie up that many
    // runtime worker threads and stall heartbeats, claims, and log streaming.
    let in_use = shared.srcdest.in_use();
    {
        let cache = cache.clone();
        // Detached, never awaited: awaiting it would make every job start
        // wait for the full tree scan the comment above promises never blocks
        // the build. Best-effort either way — the next job's scan retries
        // whatever this one misses. The watcher only logs a panic; it holds
        // no build resource and unblocks nothing.
        let evict = tokio::task::spawn_blocking(move || {
            cache.evict(&in_use);
        });
        tokio::spawn(async move {
            let _ = join_cache_task(evict, "srcdest eviction").await;
        });
    }

    // Defensive: a crash mid-job could have left a tree behind under this id.
    // Only this job's configs live here now; its source and packages are in
    // its own subvolume in the pool, under its disk quota (step 4).
    let _ = std::fs::remove_dir_all(workdir);

    // 1.5. Reconcile the shared package cache against what the served
    // repository publishes *now* (see design/implemented/stale-shared-pacman-cache.md).
    // The stale bytes live in the shared cache, the second, read-only
    // `CacheDir`: pacman finds them there, fails the integrity check at
    // install, and — being on a read-only mount — cannot delete them, so the
    // dependency install aborts. Removed here, before the chroot is even
    // prepared. The only writes that cache ever sees are this worker's, so
    // nothing else can race the reconcile.
    //
    // A fetch/parse failure is a warning, not an error: the build proceeds
    // without a reconcile, and the promote step's own re-fetch stands in for it
    // when it can. The one thing the code must never do is reconcile against a
    // previous fetch's stale copy.
    match repo_db::fetch_repo_db(client, client.repo_section(), &job.arch).await {
        Ok(db) => {
            let cache = cache.clone();
            let removed = join_cache_task(
                tokio::task::spawn_blocking(move || cache.reconcile_pkgs(&db)),
                "shared-cache reconcile",
            )
            .await
            .unwrap_or(0);
            if removed > 0 {
                tracing::info!("reconciled shared package cache: removed {removed} stale file(s)");
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "skipping shared-cache reconcile");
        }
    }

    // 2. Stage build credentials, then write per-package configs + ensure base
    //    chroot. The key is read now rather than at worker start, so replacing
    //    it on the host takes effect on the next build without a restart.
    let secrets_dir = credentials::secrets_dir(&cfg.core.data_dir);
    let credential =
        credentials::stage_for_job(cfg, &secrets_dir).context("staging build credentials")?;
    if credential.is_some() {
        log(
            client,
            build_id,
            "[worker] build ssh credential available\n",
        )
        .await;
    }

    let cfg_dir = workdir.join("config");
    // The server sends no `[repo]` section; the worker appends one rendered for
    // the host it reaches the server on (see aurcache_worker_core::repo).
    let pacman_conf =
        aurcache_worker_core::repo::append_to_pacman_conf(&job.pacman_conf, client.repo_section());
    let chroot::StagedConfigs {
        makepkg_overrides,
        pacman_conf,
    } = chroot::write_configs(
        &cfg_dir,
        &build::with_parallelism(
            &credentials::augment_makepkg_conf(
                &job.makepkg_conf,
                credential.as_ref(),
                agent_socket().as_deref(),
            ),
            parallelism_cpus(&cfg.build_limits, &cfg.total_build_limits),
        ),
        &pacman_conf,
        mirrorlist,
        cache.pacman_pkg().as_deref(),
    )
    .context("writing job configs")?;

    log(client, build_id, "[worker] preparing chroot\n").await;
    shared
        .chroots
        .refresh(&pacman_conf, Some((client, build_id)))
        .await
        .context("preparing base chroot")?;

    // Staged for this build alone; `makechrootpkg` installs it into the chroot
    // copy it makes, which is what keeps a per-package `makepkg_conf` from
    // reaching another build. Writing it into the shared base chroot -- which
    // is where it went until concurrent builds started overwriting each
    // other's -- is what that replaced.
    let dropin =
        chroot::stage_makepkg_dropin(&makepkg_overrides).context("staging makepkg overrides")?;

    // 3. Stage this job's trusted PGP keys into a keyring of its own, copied
    //    from the shared one so a key is fetched from the keyserver once and
    //    then reused, without the build reading a keybox that a sibling job's
    //    import may rewrite underneath it.
    if let (Some(shared), Some(replica)) = (cache.gnupg_home(), cache.gnupg_job(&job_label)) {
        chroot::prepare_job_keyring(&shared, &replica, &cfg.keyserver, &job.pgp_keys)
            .await
            .ok();
    }

    let binds = prepare_bind_mounts(cfg, client, job, shared, &cache, &job_label).await;
    Ok(PreparedJob {
        cache,
        dropin,
        binds,
        cleanup,
    })
}

/// Plan operator and per-job mounts, reclaiming persistent trees before use.
async fn prepare_bind_mounts(
    cfg: &Config,
    client: &Arc<WorkerClient>,
    job: &JobDescriptor,
    shared: &Shared,
    cache: &Cache,
    job_label: &str,
) -> Vec<BindMount> {
    // Private writable pacman cache for this job. Concurrent builds otherwise
    // share one writable cache directory and race on the same partial
    // download; the shared cache remains available read-only for hits.
    let build_id = job.build_id;
    let pkg_cache_bind = cache.pacman_pkg_job(job_label).map(|dir| BindMount {
        host: dir,
        chroot: PathBuf::from(chroot::PER_JOB_CACHE_MOUNT),
    });

    // Operator-configured mounts, then this job's private pacman cache. Ours
    // land last on the systemd-nspawn command line, which is what lets the
    // per-job cache override arch-nspawn's own bind of the shared one.
    //
    // The build credential is deliberately *not* here. `makepkg` fetches
    // sources on the worker before the chroot is entered, so the key is never
    // needed inside it — and binding it in would hand it to whatever the
    // PKGBUILD chooses to run there.
    let mut binds = cfg.bind_mounts.clone();
    binds.extend(pkg_cache_bind);
    // A build tree that outlives the chroot, for packages that asked for one.
    //
    // Bound over `/build`, which is where `makechrootpkg` points `BUILDDIR`, so
    // makepkg lays out `$BUILDDIR/$pkgbase/src` inside it exactly as it would
    // otherwise -- the tree simply survives the copy being deleted. makepkg
    // only clears `$srcdir` under `--cleanbuild`, which nothing here passes, so
    // an existing tree is kept and sources are re-extracted over it.
    //
    // Only when the package opted in: reuse trades away the clean tree a chroot
    // build otherwise guarantees. See `design/implemented/persistent-build-directory.md`.
    if job.persistent_builddir {
        // Before the build, so the reserve is what bounds usage going in
        // rather than a post-hoc tidy. Never drops a tree a build is using:
        // this one's, or a sibling's still running (the set includes both).
        let in_use = shared.srcdest.in_use();
        // On the blocking pool: deleting a tree of millions of files takes
        // minutes, and this build waits for it anyway.
        let (reclaim_cache, arch) = (cache.clone(), job.arch.clone());
        let (max_bytes, min_free) = (cfg.core.builddir_max_bytes, cfg.core.builddir_min_free);
        let _ = join_cache_task(
            tokio::task::spawn_blocking(move || {
                reclaim_cache.reclaim_builddirs(&arch, &in_use, max_bytes, min_free);
            }),
            "builddir reclaim",
        )
        .await;
        // Only this package's tree, at the path makepkg uses for it
        // (`$BUILDDIR/$pkgbase`). Binding the whole per-arch directory, as
        // this did, handed every build every other package's tree to read
        // and rewrite.
        if let Some(tree) = cache.builddir_tree(&job.arch, &job.pkgbase) {
            binds.push(BindMount {
                host: tree,
                chroot: Path::new(chroot::BUILDDIR_MOUNT).join(&job.pkgbase),
            });
        } else {
            let msg = format!(
                "{} asked for a persistent build directory but one could not be \
                 prepared; building in the chroot's own /build instead",
                job.pkgbase
            );
            tracing::warn!("{msg}");
            report_warning(client, Some(build_id), &msg).await;
        }
    }
    // The ssh-agent socket, so a PKGBUILD that fetches from a private
    // repository can authenticate *inside* the chroot.
    //
    // Not a hole in the credential design but the point of it: this used to be
    // withheld because the alternative was mounting the key itself, which a
    // build could then read and keep. The agent removed that -- it hands out
    // signatures, never key material -- so the build gets the use of the
    // credential and none of the possession. Agent hijacking for the duration
    // of a build remains possible and accepted, as it already was for source
    // fetching on the worker.
    //
    // Bound at the same path inside, because `SSH_AUTH_SOCK` is inherited from
    // the worker and has to name something that exists in both.
    //
    // The clone `unreal-engine` needs is in `prepare()`, not `source=`: makepkg
    // has no shallow-clone support and the engine's history is enormous, so
    // packages of that shape do their own clone inside the chroot. Assuming
    // authenticated fetches only ever happen through `source=` was what left
    // that unbuildable.
    if let Some(dir) = agent_socket().as_deref().and_then(Path::parent) {
        binds.push(BindMount {
            host: dir.to_path_buf(),
            chroot: dir.to_path_buf(),
        });
    }

    binds
}

/// Download sources, run the build and upload its successful artifacts.
async fn execute_prepared_job(
    ctx: &WorkerContext<'_>,
    client: &Arc<WorkerClient>,
    job: &JobDescriptor,
    cancel: &AtomicBool,
    prepared: &PreparedJob,
) -> Result<CompleteReport> {
    let build_id = job.build_id;

    log(client, build_id, "[worker] downloading source\n").await;
    let source = client
        .source(build_id)
        .await
        .context("downloading source")?;
    let pkgdir =
        artifacts::extract_source(&source, &ctx.lease.srcdir()).context("extracting source")?;

    // The archive unpacks owned by this worker's user, with the modes
    // baked into it (0644/0755); the build then runs as a *different*
    // user (`build_user`). makepkg rewrites the PKGBUILD in place for
    // VCS packages (`pkgver()` at build time), and its `update_pkgver`
    // prints a warning and proceeds with the stale version whenever the
    // file is not writable — the `ttf-google-fonts-git` symptom of a
    // pkgver that never advances. The docker builder needs the same and
    // solves it with `chmod -R a+w .`.
    make_source_writable(&pkgdir)
        .await
        .context("making source writable")?;

    let execution = execute_build_process(
        ctx,
        client,
        job,
        &pkgdir,
        &prepared.cache,
        &prepared.binds,
        cancel,
    )
    .await?;
    let report = finish_build_execution(ctx, client, job, &prepared.cache, execution).await;
    // Before the lease goes, and the packages with it.
    if report.outcome == BuildOutcome::Succeeded {
        upload_artifacts(client, build_id, &pkgdir).await?;
    }
    Ok(report)
}

/// Keep a failed build for inspection or release its lease on every other outcome.
async fn settle_build_lease(
    cfg: &Config,
    client: &Arc<WorkerClient>,
    job: &JobDescriptor,
    shared: &Shared,
    cache: &Cache,
    lease: Lease,
    outcome: Result<CompleteReport>,
) -> Result<CompleteReport> {
    let build_id = job.build_id;
    let mut report = match outcome {
        Ok(report) => report,
        Err(e) => {
            shared.chroots.release(lease).await;
            return Err(e);
        }
    };
    // A failed build's chroot is kept for inspection when keeping is on and
    // the failure is keepable. The keep consumes the lease; anything else
    // releases it.
    let keep_for = match cfg.keep_failed.map(Duration::from_secs) {
        Some(keep_for) if should_keep(build_id, &report) => shared
            .chroots
            .disk_reason(&lease)
            .await
            .is_none()
            .then_some(keep_for),
        _ => None,
    };
    if let Some(keep_for) = keep_for {
        // The persistent tree goes with the keep: the pool moves it beside the
        // kept chroot, after the chroot itself, so the operator gets the state
        // the build failed in and the package's next build finds no tree.
        let tree = job
            .persistent_builddir
            .then(|| cache.builddir_tree_path(&job.arch, &job.pkgbase));
        match shared.chroots.keep(lease, keep_for, tree.as_deref()).await {
            Ok(kept) => {
                let tree = kept.tree.as_ref().map(|t| t.display().to_string());
                report.kept = Some(aurcache_common::api::builds::KeptBuild {
                    path: kept.path.display().to_string(),
                    until: kept
                        .until
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
                        .unwrap_or(0),
                    tree: tree.clone(),
                });
                let mut msg = format!("[worker] failed chroot kept at {}", kept.path.display());
                if let Some(tree) = tree {
                    msg.push_str(&format!(" with its build tree at {tree}"));
                }
                msg.push('\n');
                log(client, build_id, &msg).await;
            }
            Err(e) => {
                log(
                    client,
                    build_id,
                    &format!("[worker] could not keep the failed chroot ({e:#}); deleted it\n"),
                )
                .await;
            }
        }
    } else {
        shared.chroots.release(lease).await;
    }
    Ok(report)
}

/// Discard an unsuccessful persistent tree so a retry starts cold.
async fn discard_failed_tree(
    client: &Arc<WorkerClient>,
    job: &JobDescriptor,
    cache: &Cache,
    succeeded: bool,
) {
    let build_id = job.build_id;
    // An unsuccessful build never leaves its tree for the next one: whatever
    // the keep did not take -- no keep, a keep that failed, a tree that could
    // not move -- goes here. See `Cache::discard_builddir` for why.
    if !succeeded && job.persistent_builddir {
        let (cache, arch, pkgbase) = (cache.clone(), job.arch.clone(), job.pkgbase.clone());
        let discarded = join_cache_task(
            tokio::task::spawn_blocking(move || cache.discard_builddir(&arch, &pkgbase)),
            "builddir discard",
        )
        .await
        .unwrap_or_default();
        if discarded == crate::cache::Discard::Removed {
            log(
                client,
                build_id,
                "[worker] discarded the build tree; the next retry starts cold\n",
            )
            .await;
        }
    }
}

/// Promote completed downloads and remove this job's private cache replicas.
async fn finish_package_cache(
    client: &Arc<WorkerClient>,
    job: &JobDescriptor,
    cache: &Cache,
    job_label: &str,
) {
    let build_id = job.build_id;
    // 5. Fold this job's downloads into the shared cache, then bound it. Only
    //    after `makechrootpkg` has exited: promoting mid-build would move
    //    packages out from under the running pacman.
    //
    //    Re-fetched fresh, not the reconcile's copy: the build may have taken
    //    minutes, and a dependency rebuilt mid-build is exactly the case the
    //    promote-time check exists for. The fetch is conditional, so an
    //    unchanged DB costs one cheap round-trip.
    let repo_db = match repo_db::fetch_repo_db(client, client.repo_section(), &job.arch).await {
        Ok(db) => Some(db),
        Err(e) => {
            // No DB to validate against: promote anyway (never silently drop
            // artifacts), but record the promotions as unverified so the next
            // reconcile re-hashes them instead of trusting them.
            tracing::warn!(error = %e, "promoting without a repository DB to validate against");
            report_warning(
                client,
                Some(build_id),
                &format!("promoting without a repository DB to validate against: {e}"),
            )
            .await;
            None
        }
    };
    {
        let cache = cache.clone();
        let label = job_label.to_owned();
        let _ = join_cache_task(
            tokio::task::spawn_blocking(move || {
                let promoted = cache.promote_job_pkgs(&label, repo_db.as_deref());
                cache.wipe_pacman_pkg_job(&label);
                cache.wipe_gnupg_job(&label);
                let evicted = cache.evict_pkgs();
                if promoted > 0 || !evicted.is_empty() {
                    tracing::info!(
                        "package cache: promoted {promoted}, evicted {}",
                        evicted.len()
                    );
                }
            }),
            "package-cache promote",
        )
        .await;
    }
}

/// stdout and stderr differ in type but not in handling.
enum Either {
    Out(tokio::process::ChildStdout),
    Err(tokio::process::ChildStderr),
}

/// Recursively open up the extracted source tree so the build user can write
/// to it. `makechrootpkg` bind-mounts the package directory into the chroot
/// and runs `makepkg` as `build_user`, which is not the user that unpacked
/// the archive: makepkg rewrites the PKGBUILD in place when a `pkgver()`
/// function is present, and it only does so where the file is writable (see
/// `update_pkgver` in `makepkg.sh.in`) — otherwise it warns and builds with
/// the stale version. The directory lives in the per-job `workdir`, which
/// `run_job` removes on every exit path, so the loosened permissions do not
/// outlive the build.
async fn make_source_writable(pkgdir: &Path) -> Result<()> {
    let status = tokio::process::Command::new("chmod")
        .arg("-R")
        .arg("a+w")
        .arg(pkgdir)
        .status()
        .await
        .context("running chmod -R a+w")?;
    if !status.success() {
        anyhow::bail!("chmod -R a+w {} failed: {status}", pkgdir.display());
    }
    Ok(())
}

/// How much output to accumulate before sending, and how long to hold a
/// partial batch. A build emits thousands of lines; one request per line would
/// swamp the server, and one request at the end would defeat the purpose.
const LOG_BATCH_BYTES: usize = 4096;
const LOG_BATCH_INTERVAL: Duration = Duration::from_millis(500);
/// Largest single read from the child: a newline-less flood must arrive in
/// capped chunks, never as one unbounded `read_line` allocation. Chunk
/// boundaries are invisible downstream — the log is append-only.
const LOG_LINE_CAP_BYTES: usize = 1024 * 1024;

/// Forward a child stream to the build log, batched.
///
/// Batches flush on size or age, whichever comes first, and always on EOF — so
/// the last lines of a failed build, which are the ones that explain it, are
/// never left in the buffer.
async fn pump_output<R>(reader: R, client: Arc<WorkerClient>, build_id: i32)
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::{AsyncBufReadExt, AsyncReadExt};

    let mut reader = tokio::io::BufReader::new(reader);
    let mut batch = String::new();
    let mut line = Vec::new();
    let mut last_flush = std::time::Instant::now();

    loop {
        line.clear();
        // Bytes, decoded lossily: build output is not always UTF-8 (and a
        // capped chunk can end mid-character), and `read_line` would treat
        // either as a read error -- ending the log and leaving the child
        // writing into a pipe nobody drains.
        match (&mut reader)
            .take(LOG_LINE_CAP_BYTES as u64)
            .read_until(b'\n', &mut line)
            .await
        {
            Ok(0) => break,
            Ok(_) => {
                let text = String::from_utf8_lossy(&line);
                batch.push_str(&text);
                if batch.len() >= LOG_BATCH_BYTES || last_flush.elapsed() >= LOG_BATCH_INTERVAL {
                    log(&client, build_id, &batch).await;
                    batch.clear();
                    last_flush = std::time::Instant::now();
                }
            }
            // A read error ends the stream; the build's own exit status is
            // what determines success, so this is not escalated.
            Err(e) => {
                tracing::debug!("build {build_id}: reading output ended: {e}");
                break;
            }
        }
    }

    if !batch.is_empty() {
        log(&client, build_id, &batch).await;
    }
}

/// The build ssh-agent's socket, if this worker started one.
///
/// Read from the environment because that is where the worker publishes it at
/// startup, and it is the same value the bind mount and the chroot's
/// `makepkg.conf` drop-in both have to name.
fn agent_socket() -> Option<PathBuf> {
    let sock = PathBuf::from(std::env::var_os("SSH_AUTH_SOCK")?);
    sock.exists().then_some(sock)
}

/// What this worker brings to a build, as against what the job describes.
///
/// Grouped because they travel together and are both "how this machine builds"
/// rather than "what is being built".
struct WorkerContext<'a> {
    cfg: &'a Config,
    cgroups: Option<&'a Hierarchy>,
    /// This build's chroot, which names itself to `makechrootpkg`.
    lease: &'a Lease,
    /// Where the lease came from, to tell whether a failed build ran out of
    /// its disk quota.
    chroots: &'a crate::chroots::Chroots,
    /// This build's makepkg overrides, which `makechrootpkg` installs into the
    /// chroot copy it makes for it.
    dropin: &'a Path,
}

/// How long a killed build's output may stay open before the build is reported
/// ended anyway. Generous: a killed tree closes its pipes within milliseconds,
/// so reaching this means something escaped the kill, not that it was slow.
const KILLED_OUTPUT_GRACE: Duration = Duration::from_secs(30);

/// Spawn `makechrootpkg`, stream its output as logs, and poll for local
/// self-abort, a remote cancel request, and the build timeout — killing the
/// child in any of those cases.
///
/// The output really is streamed: stdout and stderr are piped and forwarded in
/// batches. Letting the child inherit the worker's stdio instead sends the
/// build's output to the worker's own container logs, where the user reading
/// the build page cannot see it.
/// The completed process and its resource report, ready for artifact handling.
struct BuildExecution {
    status: std::process::ExitStatus,
    stopped: Option<Stop>,
    elapsed: Duration,
    cgroup: Option<crate::cgroup::BuildCgroup>,
    total_ooms_before: Option<u64>,
}

async fn execute_build_process(
    ctx: &WorkerContext<'_>,
    client: &Arc<WorkerClient>,
    job: &JobDescriptor,
    pkgdir: &Path,
    cache: &Cache,
    binds: &[BindMount],
    cancel: &AtomicBool,
) -> Result<BuildExecution> {
    let build_id = job.build_id;
    let srcdest = cache.srcdest(&job.pkgbase);
    let gnupg = cache.gnupg_job(&format!("job-{build_id}"));
    let argv = build::build_command(
        ctx.lease.chroot_dir(),
        ctx.lease.label(),
        binds,
        &job.build_flags,
        &ctx.cfg.build_user,
    );

    tracing::debug!("$ sudo {}", argv.join(" "));
    // One cgroup for this build. The child places itself into it between fork
    // and exec, so every descendant -- nspawn, makepkg, each compiler -- is
    // accounted to it, and `memory.peak` is the whole tree's high-water mark.
    //
    // A hierarchy the worker could not prepare means no figure, never a failed
    // build: this measures the work, it does not do it -- unless limits are
    // configured, which are enforced here or not at all, and a build does not
    // run without the limits its operator set.
    let limits = &ctx.cfg.build_limits;
    if ctx.cgroups.is_none() && !(limits.is_empty() && ctx.cfg.total_build_limits.is_empty()) {
        anyhow::bail!(
            "build resource limits (WORKER_BUILD_* or WORKER_TOTAL_BUILD_*) are set, but \
             this worker has no cgroup to enforce them in (see the worker's startup log)"
        );
    }
    let build_cgroup = ctx
        .cgroups
        .map(|h| h.for_build(build_id, limits))
        .transpose()
        .context("applying the build's resource limits")?;
    // The total's OOM count is cumulative over every build; what this build
    // needs is whether it moved while it ran.
    let total_ooms_before = ctx.cgroups.and_then(Hierarchy::total_ooms);
    let spawn = |argv: &[String]| {
        let mut cmd = chroot::devtools_in(&argv[0], &ctx.lease.tmpdir());
        cmd.args(&argv[1..]).current_dir(pkgdir).kill_on_drop(true);
        // The build's own process group: alone it enables the process-group
        // SIGKILL fallback below, which takes makechrootpkg's whole tree even
        // where no cgroup was prepared.
        cmd.process_group(0);
        if let Some(cg) = &build_cgroup {
            // Opened per spawn: the retry below runs this closure again, and a
            // descriptor consumed by the first attempt is gone by then.
            let handle = cg.procs_handle().map_err(std::io::Error::other)?;
            // SAFETY: `join_current_process` is one write to a descriptor
            // opened before the fork, which is async-signal-safe. Nothing else
            // runs in this hook.
            unsafe {
                cmd.pre_exec(move || crate::cgroup::BuildCgroup::join_current_process(&handle));
            }
            // Keep the container in that cgroup too. Left to itself,
            // systemd-nspawn moves it into a scope of its own under
            // devtools.slice, where neither `cgroup.kill` nor `memory.peak`
            // reaches -- a stopped build kept compiling, and the figure was
            // only the host-side download. The patched makechrootpkg turns this
            // into `--keep-unit`; see `packaging/patch-makechrootpkg.py`.
            // Only with a cgroup: without one, the unit nspawn would keep is
            // the worker's own service.
            cmd.env("AURCACHE_NSPAWN_KEEP_UNIT", "1");
        }
        // Capture the build's output instead of letting it inherit the
        // worker's stdio. Without this the log a user sees ends at
        // "[worker] starting build" and everything makepkg prints — including
        // the error that explains a failure — only reaches the worker's
        // container logs.
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        // devtools binds `$SRCDEST` into the chroot itself; without it set, it
        // falls back to the PKGBUILD directory and nothing is cached.
        if let Some(dir) = srcdest.as_deref() {
            cmd.env("SRCDEST", dir);
        }
        // `makechrootpkg` verifies source signatures on the worker, before the
        // chroot is entered, and reaches that step through
        // `sudo --preserve-env=GNUPGHOME` -- which forwards nothing unless the
        // variable is in this command's environment. Unset, gpg falls back to
        // `~builder/.gnupg`, a directory the build user cannot create, and dies
        // before it reports anything; makepkg turns that silence into
        // "SIGNATURE NOT FOUND", which names neither gpg nor the real cause.
        if let Some(dir) = gnupg.as_deref() {
            cmd.env("GNUPGHOME", dir);
        }
        // This build's makepkg overrides. `makechrootpkg` installs the file
        // named here into the chroot copy it makes for this build, so no two
        // builds share one -- and the host-side source download reads the same
        // copy, which a bind mount inside the container would not reach.
        cmd.env("AURCACHE_DROPIN", ctx.dropin);
        cmd.spawn()
    };

    let mut child = match spawn(&argv) {
        Ok(c) => c,
        Err(e) => {
            // Self-heal: a corrupt cache dir can break the first spawn; wipe and
            // retry once cold.
            cache.wipe_srcdest(&job.pkgbase);
            tracing::warn!("build spawn failed ({e}); retrying cold");
            report_warning(
                client,
                Some(build_id),
                &format!("build spawn failed ({e}); retrying cold"),
            )
            .await;
            spawn(&argv).context("spawning build")?
        }
    };

    // Watched for cancellation and the timeout while the child runs; see
    // `StopWatch` for how often each is looked at.
    let mut watch = StopWatch::start(ctx.cfg.core.build_timeout, cancel, build_id);
    // Forward both streams to the build log. Held so they can be awaited after
    // the child exits, which is what guarantees the final lines are sent.
    let pumps = [
        child.stdout.take().map(Either::Out),
        child.stderr.take().map(Either::Err),
    ]
    .into_iter()
    .flatten()
    .map(|stream| {
        let client = Arc::clone(client);
        tokio::spawn(async move {
            match stream {
                Either::Out(r) => pump_output(r, client, build_id).await,
                Either::Err(r) => pump_output(r, client, build_id).await,
            }
        })
    })
    .collect::<Vec<_>>();

    let mut stopped = None;
    let status = loop {
        tokio::select! {
            res = child.wait() => break res.context("awaiting build")?,
            () = tokio::time::sleep(StopWatch::TICK) => {
                stopped = watch.check(client).await;
                if stopped.is_some() {
                    // Kill the whole build tree, not just makechrootpkg:
                    // cgroup.kill (recursive over descendants) when the cgroup
                    // is available, else the process-group SIGKILL the
                    // `process_group(0)` spawn made possible. `start_kill`
                    // alone would orphan the compilers and whatever the chroot
                    // spawned into a systemd-managed scope.
                    let killed = if let Some(cg) = &build_cgroup {
                        match cg.kill() {
                            Ok(()) => true,
                            Err(e) => {
                                let msg = format!(
                                    "cgroup.kill failed for build {build_id}: {e:#}; \
                                     falling back to the process group"
                                );
                                tracing::warn!("{msg}");
                                report_warning(client, Some(build_id), &msg).await;
                                false
                            }
                        }
                    } else {
                        false
                    };
                    if !killed && let Err(e) = kill_process_group(&mut child).await {
                        let msg = format!(
                            "process-group kill failed for build {build_id}: {e}; \
                             falling back to start_kill"
                        );
                        tracing::warn!("{msg}");
                        report_warning(client, Some(build_id), &msg).await;
                        let _ = child.start_kill();
                    }
                    let status = child.wait().await.context("awaiting killed build")?;
                    break status;
                }
            }
        }
    };

    // Drain the output before reporting: the pumps hold whatever the build
    // printed last, which is exactly what a failure report needs.
    //
    // A pump ends at EOF, which only comes once *every* holder of the pipe has
    // exited. After a normal exit that is at once. After a kill it may never
    // be, if something outlived it -- a container that escaped the cgroup did
    // exactly that, and the build stayed "active" with its lease held until the
    // process was stopped by hand. So a killed build waits a bounded time.
    let drain = async {
        for pump in pumps {
            let _ = pump.await;
        }
    };
    if stopped.is_some() {
        if tokio::time::timeout(KILLED_OUTPUT_GRACE, drain)
            .await
            .is_err()
        {
            tracing::warn!(
                "build {build_id} was killed but its output is still open after {}s; \
                 something outlived the kill. Reporting it ended regardless",
                KILLED_OUTPUT_GRACE.as_secs()
            );
        }
    } else {
        drain.await;
    }

    Ok(BuildExecution {
        status,
        stopped,
        elapsed: watch.elapsed(),
        cgroup: build_cgroup,
        total_ooms_before,
    })
}

/// Classify the process outcome and collect its final resource measurements.
async fn finish_build_execution(
    ctx: &WorkerContext<'_>,
    client: &WorkerClient,
    job: &JobDescriptor,
    cache: &Cache,
    execution: BuildExecution,
) -> CompleteReport {
    let canceled = execution.stopped == Some(Stop::Canceled);
    let timed_out = execution.stopped == Some(Stop::TimedOut);
    let mut report = if timed_out {
        report::timeout_failure(execution.elapsed.as_secs())
    } else {
        report::classify_exit(execution.status, canceled)
    };
    // Measure the tree now rather than during the next reclaim: the cost rides
    // on a build that already took minutes, instead of walking every candidate
    // on every future build. Only for a success: a failure's tree is moved
    // beside the keep or discarded by the caller, after the keep decision, and
    // measuring it first would walk a tree about to go.
    if job.persistent_builddir && report.outcome == BuildOutcome::Succeeded {
        let (cache, arch, pkgbase) = (cache.clone(), job.arch.clone(), job.pkgbase.clone());
        let _ = join_cache_task(
            tokio::task::spawn_blocking(move || cache.record_builddir_size(&arch, &pkgbase)),
            "builddir size record",
        )
        .await;
    }

    report_build_resources(ctx, client, job, cache, &execution, &mut report).await;
    report
}

/// Attach measured resources and explain quota failures in the completed report.
async fn report_build_resources(
    ctx: &WorkerContext<'_>,
    client: &WorkerClient,
    job: &JobDescriptor,
    cache: &Cache,
    execution: &BuildExecution,
    report: &mut CompleteReport,
) {
    let build_cgroup = execution.cgroup.as_ref();
    let stopped = execution.stopped;
    let total_ooms_before = execution.total_ooms_before;
    let build_id = job.build_id;
    let srcdest = cache.srcdest(&job.pkgbase);
    let canceled = stopped == Some(Stop::Canceled);
    let timed_out = stopped == Some(Stop::TimedOut);
    // Attached after the fact rather than threaded through every constructor:
    // how much a build used is orthogonal to why it ended, and a timeout or a
    // cancellation is exactly when the number is most worth having.
    report.peak_memory_bytes = build_cgroup.and_then(crate::cgroup::BuildCgroup::peak_bytes);
    report.disk_usage = disk_usage(ctx, cache, job).await;
    // What the build was actually made from, while the job still holds its
    // SRCDEST guard and no sibling can have fetched into the mirror. Only for a
    // success: nothing else is ever consulted as a baseline, and a failure has
    // no business moving one.
    if report.outcome == BuildOutcome::Succeeded {
        report.vcs_commits =
            crate::built_sources::resolve(&job.vcs_sources, srcdest.as_deref()).await;
    }
    let mut out_of_memory = false;
    // A process the kernel killed for memory looks, from makepkg's exit code,
    // like any other failure -- a compiler "terminated by signal", an error
    // several screens up the log. Say what happened where it is looked for.
    if report.outcome != BuildOutcome::Succeeded
        && !canceled
        && !timed_out
        && let Some(kills) = build_cgroup
            .and_then(crate::cgroup::BuildCgroup::oom_kills)
            .filter(|&n| n > 0)
    {
        let build_ooms = build_cgroup
            .and_then(crate::cgroup::BuildCgroup::limit_ooms)
            .unwrap_or(0);
        let total_ooms_during = ctx
            .cgroups
            .and_then(Hierarchy::total_ooms)
            .zip(total_ooms_before)
            .map_or(0, |(after, before)| after.saturating_sub(before));
        let reason = oom_reason(
            kills,
            crate::cgroup::OomCause::classify(build_ooms, total_ooms_during),
            ctx.cfg,
        );
        log(client, build_id, &format!("\n[worker] {reason}\n")).await;
        report.reason = Some(reason);
        out_of_memory = true;
    }
    // Running out of disk looks like any other failure too: whichever write
    // hit the quota fails with "Disk quota exceeded", somewhere in the log.
    // The quota group's own figures say whether that is what happened.
    if report.outcome != BuildOutcome::Succeeded
        && !canceled
        && !timed_out
        && !out_of_memory
        && let Some(reason) = ctx.chroots.disk_reason(ctx.lease).await
    {
        log(client, build_id, &format!("\n[worker] {reason}\n")).await;
        report.reason = Some(reason);
    }
}

/// What the build used on disk, part by part, as it ended; `None` when nothing
/// could be measured. After the build tree's size is recorded, so a kept tree
/// that is a plain directory reports its fresh figure.
async fn disk_usage(
    ctx: &WorkerContext<'_>,
    cache: &Cache,
    job: &JobDescriptor,
) -> Option<aurcache_common::api::builds::DiskUsage> {
    /// The package's caches, measured off the async runtime.
    #[derive(Default)]
    struct CacheSizes {
        sources: Option<u64>,
        build_tree: Option<u64>,
    }

    let build = ctx.chroots.build_usage(ctx.lease).await;
    let (cache, arch, pkgbase) = (cache.clone(), job.arch.clone(), job.pkgbase.clone());
    let keeps_tree = job.persistent_builddir;
    let cached = join_cache_task(
        tokio::task::spawn_blocking(move || CacheSizes {
            sources: cache.source_size(&pkgbase),
            build_tree: keeps_tree
                .then(|| cache.build_tree_size(&arch, &pkgbase))
                .flatten(),
        }),
        "disk usage",
    )
    .await
    .unwrap_or_default();
    let bytes = |b: Option<u64>| b.and_then(|b| i64::try_from(b).ok());
    let usage = aurcache_common::api::builds::DiskUsage {
        chroot: bytes(build.chroot),
        workdir: bytes(build.data),
        sources: bytes(cached.sources),
        build_tree: bytes(cached.build_tree),
    };
    (!usage.is_empty()).then_some(usage)
}

/// Whether a finished build's chroot is worth keeping for inspection: a real
/// failure, not a success, a cancel, a timeout, or the fixed one-shot id
/// (whose next run would collide with the kept name).
///
/// Out-of-disk builds are excluded by the caller, which still holds the lease
/// to ask: their chroot is the largest and says the least, since the cause is
/// already known. Out-of-memory failures are kept: small on disk, and the
/// state they died in is the interesting part.
fn should_keep(build_id: i32, report: &CompleteReport) -> bool {
    // Exit 124 is the worker's own timeout convention (`timeout_failure`, and
    // `classify_exit`'s mapping of a bare 124): a build that genuinely exited
    // 124 reads as timed out everywhere already.
    const TIMEOUT_EXIT: i32 = 124;
    report.outcome == BuildOutcome::Failed
        && report.exit_code != Some(TIMEOUT_EXIT)
        && build_id != crate::chroots::ONE_SHOT_BUILD_ID
}

/// The CPUs one build's parallelism is sized to: the smaller of its own limit
/// and the total, since a single build can use no more than either.
fn parallelism_cpus(
    build: &crate::cgroup::BuildLimits,
    total: &crate::cgroup::BuildLimits,
) -> Option<f64> {
    match (build.cpus, total.cpus) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// Why a build the OOM killer reached failed, naming the limit that was reached
/// when it was one of the worker's.
fn oom_reason(kills: u64, cause: crate::cgroup::OomCause, cfg: &Config) -> String {
    use crate::cgroup::OomCause;
    let processes = if kills == 1 {
        "a process".to_string()
    } else {
        format!("{kills} processes")
    };
    use crate::chroots::gib;
    match (
        cause,
        cfg.build_limits.memory_max,
        cfg.total_build_limits.memory_max,
    ) {
        (OomCause::BuildLimit, Some(bytes), _) => format!(
            "out of memory: the kernel killed {processes} at the build's memory limit of \
             {} (WORKER_BUILD_MEMORY_MAX)",
            gib(bytes)
        ),
        (OomCause::TotalLimit, _, Some(bytes)) => format!(
            "out of memory: the kernel killed {processes} when the builds on this worker \
             together reached {} (WORKER_TOTAL_BUILD_MEMORY_MAX); this build may \
             have been under its own limit",
            gib(bytes)
        ),
        (OomCause::Elsewhere, _, _) => format!(
            "out of memory: the kernel killed {processes} in the build, at a limit outside \
             the worker's own (the machine, or what runs the worker)"
        ),
        // A cause without the limit it names: configuration read at startup
        // cannot disagree with the cgroups it set up, but say something true.
        _ => format!("out of memory: the kernel killed {processes} in the build"),
    }
}

/// SIGKILL the child's whole process group.
///
/// The build is spawned with `process_group(0)`, so the child is the leader of
/// a fresh group (pgid = pid) and killing the negative pid reaches every
/// descendant that inherited the group — sudo, makechrootpkg, nspawn, the
/// compilers. This is the fallback when no per-build cgroup was prepared; on a
/// systemd host the container may have been handed to a managed scope, which
/// escapes the group, and that is a documented boundary of the kill.
async fn kill_process_group(child: &mut tokio::process::Child) -> std::io::Result<()> {
    let Some(pid) = child.id() else {
        return Ok(()); // Already reaped.
    };
    // SAFETY: a plain kill(2); every argument is a value, nothing borrowed.
    let rc = unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn an_aborted_job_wipes_its_private_caches() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = Cache::new(tmp.path(), 0, 0, 0, 0);
        let packages = cache.pacman_pkg_job("job-1").unwrap();
        let keyring = cache.gnupg_job("job-1").unwrap();
        std::fs::write(packages.join("unfinished.part"), "partial download").unwrap();
        std::fs::write(keyring.join("private"), "private material").unwrap();
        let (started, holding) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let _cleanup = JobCacheCleanup {
                cache,
                label: "job-1".into(),
                active: true,
            };
            started.send(()).unwrap();
            std::future::pending::<()>().await;
        });
        holding.await.unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        tokio::time::timeout(Duration::from_secs(5), async {
            while packages.exists() || keyring.exists() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    fn with_limits(build: Option<u64>, total: Option<u64>) -> Config {
        let mut cfg = Config::from_env();
        cfg.build_limits.memory_max = build;
        cfg.total_build_limits.memory_max = total;
        cfg
    }

    /// Each cause names the variable an operator would change, and a kill at the
    /// total says the build itself may not have been over anything.
    #[test]
    fn an_oom_reason_names_the_limit_that_was_reached() {
        use crate::cgroup::OomCause;
        let cfg = with_limits(Some(8 << 30), Some(48 << 30));

        let own = oom_reason(1, OomCause::BuildLimit, &cfg);
        assert!(own.contains("a process"), "{own}");
        assert!(own.contains("8.0 GiB (WORKER_BUILD_MEMORY_MAX)"), "{own}");

        let total = oom_reason(3, OomCause::TotalLimit, &cfg);
        assert!(total.contains("3 processes"), "{total}");
        assert!(
            total.contains("48.0 GiB (WORKER_TOTAL_BUILD_MEMORY_MAX)"),
            "{total}"
        );
        assert!(total.contains("under its own limit"), "{total}");

        let elsewhere = oom_reason(1, OomCause::Elsewhere, &cfg);
        assert!(!elsewhere.contains("WORKER_"), "{elsewhere}");
    }

    /// Kept: a real failure, including an OOM kill. Not kept: a success, a
    /// cancel, a timeout, or the one-shot id. (Out-of-disk is excluded by the
    /// caller, which still holds the lease to ask.)
    #[test]
    fn only_real_failures_are_keepable() {
        use aurcache_worker_core::report;
        use std::os::unix::process::ExitStatusExt;

        let status = |code: i32| std::process::ExitStatus::from_raw(code << 8);
        assert!(should_keep(7, &report::classify_exit(status(1), false)));
        assert!(should_keep(7, &report::classify_exit(status(137), false)));
        assert!(!should_keep(7, &report::classify_exit(status(0), false)));
        assert!(!should_keep(7, &report::classify_exit(status(1), true)));
        assert!(!should_keep(7, &report::timeout_failure(60)));
        assert!(!should_keep(7, &report::classify_exit(status(124), false)));
        assert!(!should_keep(
            crate::chroots::ONE_SHOT_BUILD_ID,
            &report::classify_exit(status(1), false)
        ));
    }

    /// A finished blocking task hands its value through; a panicking one
    /// comes back as `None` (and logs) rather than failing the job — the
    /// whole point of routing the joins through one helper.
    #[tokio::test]
    async fn join_cache_task_passes_values_and_absorbs_panics() {
        let ok = join_cache_task(tokio::task::spawn_blocking(|| 40 + 2), "test").await;
        assert_eq!(ok, Some(42));
        let dead: Option<i32> = join_cache_task(
            tokio::task::spawn_blocking(|| panic!("deliberate test panic")),
            "test",
        )
        .await;
        assert_eq!(dead, None);
    }

    #[test]
    fn parallelism_follows_the_tighter_cpu_limit() {
        use crate::cgroup::BuildLimits;
        let cpus = |c: Option<f64>| BuildLimits {
            cpus: c,
            ..BuildLimits::default()
        };
        assert_eq!(
            parallelism_cpus(&cpus(Some(8.0)), &cpus(Some(6.0))),
            Some(6.0)
        );
        assert_eq!(
            parallelism_cpus(&cpus(Some(4.0)), &cpus(Some(6.0))),
            Some(4.0)
        );
        assert_eq!(parallelism_cpus(&cpus(None), &cpus(Some(6.0))), Some(6.0));
        assert_eq!(parallelism_cpus(&cpus(None), &cpus(None)), None);
    }

    /// The build user is not the worker's user; makepkg can only rewrite the
    /// PKGBUILD for a `pkgver()` check where the extracted tree is writable.
    #[tokio::test]
    async fn make_source_writable_opens_the_tree() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let pkgdir = tmp.path().join("pkg");
        std::fs::create_dir_all(pkgdir.join("sub")).unwrap();
        std::fs::write(pkgdir.join("PKGBUILD"), b"pkgname=x").unwrap();
        std::fs::write(pkgdir.join("sub/data"), b"x").unwrap();
        // As tar::unpack leaves them: 0755 dirs, 0644 files.
        std::fs::set_permissions(&pkgdir, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::set_permissions(
            pkgdir.join("PKGBUILD"),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();

        make_source_writable(&pkgdir).await.unwrap();

        for path in [&pkgdir, &pkgdir.join("sub"), &pkgdir.join("PKGBUILD")] {
            let mode = std::fs::metadata(path).unwrap().permissions().mode();
            assert_eq!(
                mode & 0o222,
                0o222,
                "{} must be writable by all",
                path.display()
            );
        }
    }
}
