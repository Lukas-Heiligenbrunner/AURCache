//! Base `devtools` chroot lifecycle and per-job execution helpers that shell
//! out to Arch's `devtools`. These run only on a real privileged worker image;
//! the pure argument/parse helpers they rely on live in [`crate::build`].

use anyhow::{Context, Result, bail};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use tokio::process::Command;
use tokio::sync::Mutex;

/// What a finished command printed, and how it exited.
pub struct Captured {
    /// stdout, then stderr.
    pub log: String,
    pub status: std::process::ExitStatus,
}

/// Run a command, capturing its combined output and its exit status.
pub async fn run_capture(mut cmd: Command) -> Result<Captured> {
    let output = cmd.output().await.context("spawning command")?;
    let mut log = String::new();
    log.push_str(&String::from_utf8_lossy(&output.stdout));
    log.push_str(&String::from_utf8_lossy(&output.stderr));
    Ok(Captured {
        log,
        status: output.status,
    })
}

/// The variables `sudo` must pass through to devtools; see [`devtools`].
const DEVTOOLS_ENV: &str =
    "--preserve-env=GNUPGHOME,SRCDEST,AURCACHE_DROPIN,AURCACHE_NSPAWN_KEEP_UNIT";

/// Build a `Command` that runs a privileged `devtools` program via `sudo`.
///
/// The worker runs as the unprivileged `aurcache` user; `sudo` provides the
/// root that mounting, unsharing and chrooting need. Which user the *build*
/// runs as is passed explicitly with `-U` rather than left to `makechrootpkg`'s
/// `SUDO_USER` inference, so it is `builder` and never `aurcache` -- see
/// `build::build_command`.
///
/// Four variables are preserved, because sudo would otherwise drop all of
/// them: `GNUPGHOME` names this build's keyring replica (see
/// [`prepare_job_keyring`]), `SRCDEST` the source cache `makechrootpkg` binds
/// itself, `AURCACHE_DROPIN` the makepkg overrides it installs into this
/// build's chroot copy, and `AURCACHE_NSPAWN_KEEP_UNIT` whether the container
/// stays in the build's cgroup. `PKGDEST` reaches the build through
/// `makepkg.conf` instead, so preserving it here would be a no-op.
pub fn devtools(program: &str) -> Command {
    let mut cmd = Command::new("sudo");
    // `SRCDEST` is how `makechrootpkg` is told where to keep downloaded
    // sources; sudo would otherwise strip it and devtools would silently fall
    // back to the PKGBUILD directory, losing the cache.
    cmd.arg(DEVTOOLS_ENV).arg(program);
    cmd
}

/// [`devtools`], with `TMPDIR` set to `tmpdir` for the program.
///
/// For a build: devtools makes its `WORKDIR` with `mktemp` under `TMPDIR`, and
/// the host-side `makepkg --verifysource` -- which sources the PKGBUILD --
/// writes there (it is that run's `BUILDDIR` and `TMPDIR`). Left at `/tmp`,
/// that is the one place a PKGBUILD could fill a disk outside the build's
/// quota. Pointed into the build's own subvolume, it counts against it.
///
/// Through `env` on the command line, because the environment cannot carry
/// it: glibc removes `TMPDIR` from a setuid program's environment, so sudo
/// never sees a preserved one. devtools also unsets a preset `WORKDIR`,
/// which is why this sets the directory `WORKDIR` is made in, not `WORKDIR`.
pub fn devtools_in(program: &str, tmpdir: &Path) -> Command {
    let mut cmd = Command::new("sudo");
    cmd.arg(DEVTOOLS_ENV)
        .arg("env")
        .arg(format!("TMPDIR={}", tmpdir.display()))
        .arg(program);
    cmd
}

/// Whether there is a chroot here already, rather than an empty directory.
pub(crate) fn base_exists(root: &Path) -> bool {
    root.join(".arch-chroot").exists() || (root.exists() && root.join("usr").exists())
}

/// `mkarchroot` a fresh base chroot, seeded with the job's `pacman.conf`. The
/// chroot keeps the `makepkg.conf` its own `pacman` installs; this worker's
/// settings arrive as a drop-in instead.
///
/// The caller counts it against the pool's total, and removes it when this
/// fails: what a failed creation leaves is neither usable nor counted, and
/// `mkarchroot` refuses to start over on top of it.
pub(crate) async fn create_base(root: &Path, pacman_conf: &Path) -> Result<()> {
    // No `-M`. `mkarchroot` would copy a makepkg.conf into the chroot, and its
    // default is the *host's* -- which is the operator's own build
    // configuration, not this service's. A developer machine with
    // `BUILDENV=(... ccache ...)` is entirely ordinary, and carrying that into a
    // clean `base-devel` chroot fails every build before it compiles anything:
    //
    //   ==> ERROR: Cannot find the ccache binary required for compiler cache usage.
    //
    // Left alone, the chroot keeps the `makepkg.conf` its own `pacman` package
    // installed: complete, correct for the chroot's architecture, and owing
    // nothing to whatever the host happens to be configured for. What this
    // worker wants to change goes in `makepkg.conf.d/` instead.
    let mut cmd = devtools("mkarchroot");
    cmd.arg("-C")
        .arg(pacman_conf)
        .arg(root)
        .arg("base-devel")
        // git+ssh sources are fetched by makepkg *inside* the chroot, and
        // base-devel carries neither git nor an ssh client.
        .arg("git")
        .arg("openssh");
    let Captured { log, status } = run_capture(cmd).await?;
    if !status.success() {
        bail!("mkarchroot failed:\n{log}");
    }
    ensure_multilib(root).await;
    Ok(())
}

/// Upgrade a refresh candidate in place: `root.next`, never the base builds
/// snapshot. Must exit 0: a failed upgrade is a failed refresh, and the
/// candidate is deleted rather than swapped in.
pub(crate) async fn upgrade_candidate(next: &Path) -> Result<()> {
    let mut cmd = devtools("arch-nspawn");
    cmd.arg(next).args(["pacman", "-Syu", "--noconfirm"]);
    let Captured { log, status } = run_capture(cmd).await?;
    if !status.success() {
        bail!("chroot upgrade returned non-zero:\n{log}");
    }
    Ok(())
}

/// Check a refresh candidate before it becomes the base, running the checks
/// inside it so they exercise the binaries just upgraded: dependencies
/// satisfied, the toolchain still installed, the build-user path working, and
/// no pacman lock left behind.
pub(crate) async fn check_candidate(next: &Path) -> Result<()> {
    let checks: &[&[&str]] = &[
        &["pacman", "-Dk"],
        &["pacman", "-Q", "base-devel"],
        &["sudo", "-u", "nobody", "true"],
    ];
    for args in checks {
        let mut cmd = devtools("arch-nspawn");
        cmd.arg(next).args(args.iter().copied());
        let Captured { log, status } = run_capture(cmd).await?;
        if !status.success() {
            bail!("chroot check `{}` failed:\n{log}", args.join(" "));
        }
    }
    if next.join("var/lib/pacman/db.lck").exists() {
        bail!("chroot check failed: /var/lib/pacman/db.lck left behind");
    }
    Ok(())
}

/// Install the 32-bit toolchain, where the architecture has one.
///
/// `base-devel` builds 64-bit only, so every `lib32-*` package -- which
/// compiles with `-m32` -- failed at configure with the singularly unhelpful
/// "C compiler cannot create executables". Sixteen of them did before this
/// existed, while the packages that happened to declare `gcc-multilib` among
/// their `makedepends` succeeded, which made it look like an intermittent
/// problem with particular packages rather than a missing toolchain.
///
/// **In a transaction of its own, and never folded into `mkarchroot` or the
/// `-Syu`.** pacman aborts an entire transaction when a single target cannot be
/// resolved, so asking for this alongside `base-devel` would mean an
/// architecture without multilib -- every one except x86_64 -- silently getting
/// no toolchain at all rather than merely no 32-bit one. The container builder
/// learned this the expensive way; `docker/add-aur.sh` carries the scar.
///
/// Best-effort for the same reason: absence is the expected case off x86_64,
/// not a failure worth refusing to build over.
pub(crate) async fn ensure_multilib(root: &Path) {
    let mut cmd = devtools("arch-nspawn");
    cmd.arg(root).args([
        "pacman",
        "-S",
        "--needed",
        "--noconfirm",
        "--noprogressbar",
        "multilib-devel",
    ]);
    match run_capture(cmd).await {
        Ok(Captured { status, .. }) if status.success() => {}
        Ok(Captured { log, .. }) => tracing::debug!(
            "multilib-devel not installed (expected off x86_64); \
             lib32-* packages will not build here:\n{log}"
        ),
        Err(e) => tracing::warn!("could not check for multilib-devel: {e:#}"),
    }
}

/// Serializes every write to the shared keyring, and every copy taken from it.
///
/// The worker is that keyring's only writer, but it runs jobs concurrently, so
/// two `--recv-keys` children would otherwise race — and worse, a replica could
/// be copied out of a keybox mid-rewrite. Held across the fetch as well as the
/// copy: a key already present is not fetched again, so once warm this is a
/// file copy's worth of contention at the start of a build.
static KEYRING_LOCK: Mutex<()> = Mutex::const_new(());

/// What a read-only GnuPG home needs to be usable.
///
/// gpg takes a dotlock in its home even to *read* a keybox, and rewrites the
/// trustdb it is handed; the build user can write neither, here or anywhere
/// else outside `SRCDEST`/`BUILDDIR` (`aurcache-sandbox` enforces that with
/// Landlock). Locking is safe to drop precisely because the replica has no
/// writer for the life of the build -- which is the whole reason it is a
/// replica.
const REPLICA_GPG_CONF: &str = "lock-never\nno-auto-check-trustdb\n";

/// Files that make up the replica. `trustdb.gpg` is not optional: gpg aborts
/// with `Fatal: can't create trustdb.gpg` on a read-only home that has a
/// keyring but no trustdb, before it reports anything about the signature.
const REPLICA_FILES: [&str; 2] = ["pubring.kbx", "trustdb.gpg"];

/// Stage the job's trusted PGP keys into a keyring of this build's own.
///
/// Keys are fetched once into `shared` and kept there, so a key some earlier
/// build already needed costs no keyserver round trip. What the build reads is
/// `replica`, a copy of that keyring: `makechrootpkg` verifies source
/// signatures *on the worker*, as the build user, while sibling jobs may be
/// importing keys — and a keybox rewritten under a reader fails the
/// verification. gpg's own dotlock does not prevent that; a reader still lands
/// between the writer's rename steps and reports `keydb_search failed: No such
/// file or directory`, measured at roughly one read in fifty during an import,
/// with locking on or off. A replica has no writer at all.
///
/// Best-effort: a key server hiccup should not abort the build if the key turns
/// out unnecessary (PKGBUILD `validpgpkeys` still gates trust).
pub async fn prepare_job_keyring(
    shared: &Path,
    replica: &Path,
    keyserver: &str,
    keys: &[String],
) -> Result<()> {
    let _guard = KEYRING_LOCK.lock().await;
    std::fs::create_dir_all(shared)
        .with_context(|| format!("creating gnupg home {}", shared.display()))?;
    for key in keys {
        if has_key(shared, key).await {
            tracing::debug!("pgp key {key} already in the shared keyring");
            continue;
        }
        let mut cmd = Command::new("gpg");
        cmd.env("GNUPGHOME", shared).args([
            "--batch",
            "--keyserver",
            keyserver,
            "--recv-keys",
            key,
        ]);
        match run_capture(cmd).await {
            Ok(Captured { status, .. }) if status.success() => {
                tracing::info!("imported pgp key {key}");
            }
            Ok(Captured { log, .. }) => tracing::warn!("could not import pgp key {key}:\n{log}"),
            Err(e) => tracing::warn!("gpg failed for key {key}: {e}"),
        }
    }
    // Ensure the trustdb exists before it is copied, rather than leaving the
    // build to discover it cannot create one. Idempotent and near-instant on a
    // keyring this size ("no need for a trustdb check" once current).
    let mut cmd = Command::new("gpg");
    cmd.env("GNUPGHOME", shared)
        .args(["--batch", "--check-trustdb"]);
    if let Ok(Captured { log, status }) = run_capture(cmd).await
        && !status.success()
    {
        tracing::warn!("could not prepare the shared trustdb:\n{log}");
    }
    // Always staged, even for a package that declares no keys: a PKGBUILD with
    // signed sources and no `validpgpkeys` then fails with "unknown public
    // key", which says what is wrong, instead of gpg dying on a home it cannot
    // write and makepkg reporting "SIGNATURE NOT FOUND".
    copy_keyring(shared, replica)
}

/// Whether the shared keyring already holds a key, so it is not fetched twice.
async fn has_key(gnupg_home: &Path, key: &str) -> bool {
    let mut cmd = Command::new("gpg");
    cmd.env("GNUPGHOME", gnupg_home)
        .args(["--batch", "--list-keys", key]);
    matches!(run_capture(cmd).await, Ok(Captured { status, .. }) if status.success())
}

/// Copy the shared keyring into a home the build user can read and nothing can
/// write.
///
/// The modes are set explicitly because `copy` carries the source's across:
/// gpg creates `trustdb.gpg` `0600`, and a replica the build cannot read fails
/// exactly like no replica at all.
fn copy_keyring(shared: &Path, replica: &Path) -> Result<()> {
    std::fs::create_dir_all(replica)
        .with_context(|| format!("creating job keyring {}", replica.display()))?;
    for name in REPLICA_FILES {
        let from = shared.join(name);
        if !from.exists() {
            continue;
        }
        let to = replica.join(name);
        std::fs::copy(&from, &to)
            .with_context(|| format!("copying {} to {}", from.display(), to.display()))?;
        set_readable(&to);
    }
    let conf = replica.join("gpg.conf");
    std::fs::write(&conf, REPLICA_GPG_CONF)
        .with_context(|| format!("writing {}", conf.display()))?;
    set_readable(&conf);
    Ok(())
}

/// Make a replica file readable by the build user, which is not the user that
/// wrote it.
fn set_readable(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644)) {
            tracing::warn!("could not make {} readable: {e}", path.display());
        }
    }
}

/// Settings `makechrootpkg` chooses for itself, which a drop-in must not touch.
///
/// It appends `BUILDDIR=/build PKGDEST=/pkgdest ...` to the chroot's
/// `/etc/makepkg.conf`, skipping any it finds already set. Those paths are
/// bind-mounted into the build; ours are not. While the server's `PKGDEST` sat
/// in that same file, makechrootpkg's line was appended after it and won -- so
/// it was always a no-op for a chroot build. From a drop-in, which makepkg
/// sources *after* the base file, it would win instead, and the build fails
/// immediately:
///
/// ```text
/// ==> ERROR: Failed to create the directory $PKGDEST (/output).
/// ```
const CHROOT_OWNED_SETTINGS: [&str; 5] =
    ["BUILDDIR", "PKGDEST", "SRCPKGDEST", "SRCDEST", "LOGDEST"];

/// Drop assignments to the settings `makechrootpkg` owns.
fn without_chroot_owned(overrides: &str) -> String {
    overrides
        .lines()
        .filter(|line| {
            let key = line.trim_start().split('=').next().unwrap_or("").trim();
            !CHROOT_OWNED_SETTINGS.contains(&key)
        })
        .map(|line| format!("{line}\n"))
        .collect()
}

/// Write this build's makepkg overrides to a file of its own, and return it.
///
/// `makechrootpkg` installs it into the job's chroot copy (see
/// `packaging/patch-makechrootpkg.py`), which is why nothing here touches the
/// base chroot. It used to: the drop-in went into the shared template before
/// every build, and `sync_chroot` carried it into the copy -- so two builds
/// starting together raced, and one could run with the other's `MAKEFLAGS`,
/// `PACKAGER` and ssh-agent socket. The copy is per build by construction,
/// and the worker cannot see when `makechrootpkg` makes it, so there is no
/// lock that would have fixed this.
pub fn stage_makepkg_dropin(staged: &Path) -> Result<PathBuf> {
    let filtered = without_chroot_owned(
        &std::fs::read_to_string(staged)
            .with_context(|| format!("reading {}", staged.display()))?,
    );
    let dropin = staged.with_extension("dropin");
    std::fs::write(&dropin, filtered).with_context(|| format!("writing {}", dropin.display()))?;
    Ok(dropin)
}

/// Append the worker's cache layout to a server-rendered `pacman.conf`.
///
/// `arch-nspawn` reads `CacheDir` from the chroot's own `pacman.conf` and
/// bind-mounts the **first** entry read-write into the container, the rest
/// read-only. Listing a private job directory first and the shared cache second
/// gives each build somewhere private to download to while still reading hits
/// from the shared pool.
///
/// The first path is a fixed mount point rather than the real per-job
/// directory: the build's `pacman.conf` is inherited from the base chroot and
/// is therefore identical for every job, so the per-job part is supplied as a
/// bind mount over that path instead (see `job.rs`).
///
/// Written worker-side rather than by the server because cache paths are
/// worker-local — the same reasoning that keeps `GIT_SSH_COMMAND` out of the
/// job descriptor.
fn with_cache_dirs(pacman_conf: &str, shared_pkg_cache: Option<&Path>) -> String {
    let Some(shared) = shared_pkg_cache else {
        return pacman_conf.to_string();
    };
    let mut out = pacman_conf.to_string();
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("# Added by aurcache-worker: per-job writable cache first (bound over by\n");
    out.push_str("# the job's private directory), shared read-only cache second.\n");
    let _ = write!(
        out,
        "[options]\nCacheDir = {PER_JOB_CACHE_MOUNT} {}\n",
        shared.display()
    );
    out
}

/// Where `makechrootpkg` points `BUILDDIR` inside the chroot. A persistent
/// build tree is bound here, so makepkg's own layout is unchanged and only the
/// lifetime differs.
pub const BUILDDIR_MOUNT: &str = "/build";

/// Mount point a job's private pacman cache is bound over. Matches pacman's
/// default so nothing else has to change.
pub const PER_JOB_CACHE_MOUNT: &str = "/var/cache/pacman/pkg";

/// Where [`write_configs`] put the files the caller goes on to install.
pub struct StagedConfigs {
    /// The makepkg overrides, for [`stage_makepkg_dropin`].
    pub makepkg_overrides: PathBuf,
    pub pacman_conf: PathBuf,
}

/// Write the per-package makepkg overrides, `pacman.conf` and mirrorlist to a
/// staging directory the caller seeds the base chroot from.
///
/// The makepkg file here is *only* the overrides, not a whole `makepkg.conf`:
/// [`stage_makepkg_dropin`] filters it into the drop-in `makechrootpkg`
/// installs in the job's chroot copy, so the chroot keeps its own defaults
/// rather than having them replaced.
pub fn write_configs(
    dir: &Path,
    makepkg_conf: &str,
    pacman_conf: &str,
    mirrorlist: Option<&str>,
    shared_pkg_cache: Option<&Path>,
) -> Result<StagedConfigs> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("creating config dir {}", dir.display()))?;
    let makepkg = dir.join("makepkg.conf");
    let pacman = dir.join("pacman.conf");

    std::fs::write(&makepkg, makepkg_conf).context("writing makepkg overrides")?;
    std::fs::write(&pacman, with_cache_dirs(pacman_conf, shared_pkg_cache))
        .context("writing pacman.conf")?;
    if let Some(list) = mirrorlist {
        std::fs::write(dir.join("mirrorlist"), list).context("writing mirrorlist")?;
    }
    Ok(StagedConfigs {
        makepkg_overrides: makepkg,
        pacman_conf: pacman,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The replica has to carry a trustdb, not just a keyring. On a home it
    /// cannot write, gpg dies with `Fatal: can't create trustdb.gpg` before it
    /// says anything about the signature -- and makepkg reports that silence
    /// as "SIGNATURE NOT FOUND".
    #[test]
    fn the_replica_carries_a_trustdb() {
        let tmp = tempfile::tempdir().unwrap();
        let shared = tmp.path().join("shared");
        let replica = tmp.path().join("job");
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::write(shared.join("pubring.kbx"), b"keys").unwrap();
        std::fs::write(shared.join("trustdb.gpg"), b"trust").unwrap();

        copy_keyring(&shared, &replica).unwrap();

        for name in REPLICA_FILES {
            assert!(replica.join(name).exists(), "{name} must reach the replica");
        }
    }

    /// gpg creates `trustdb.gpg` `0600`, and `copy` carries the source's mode
    /// across -- so an unadjusted replica is unreadable by the build user,
    /// which fails exactly like having no replica at all.
    #[test]
    fn the_replica_is_readable_by_another_user() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let shared = tmp.path().join("shared");
        let replica = tmp.path().join("job");
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::write(shared.join("trustdb.gpg"), b"trust").unwrap();
        std::fs::set_permissions(
            shared.join("trustdb.gpg"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();

        copy_keyring(&shared, &replica).unwrap();

        for name in ["trustdb.gpg", "gpg.conf"] {
            let mode = std::fs::metadata(replica.join(name))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o044, 0o044, "{name} must be readable by the build");
        }
    }

    /// Without `lock-never` gpg takes a dotlock in its home to *read* the
    /// keybox, which a read-only replica cannot grant; without
    /// `no-auto-check-trustdb` it wants to rewrite the trustdb it was handed.
    #[test]
    fn the_replica_tells_gpg_not_to_write() {
        let tmp = tempfile::tempdir().unwrap();
        let shared = tmp.path().join("shared");
        let replica = tmp.path().join("job");
        std::fs::create_dir_all(&shared).unwrap();

        copy_keyring(&shared, &replica).unwrap();

        let conf = std::fs::read_to_string(replica.join("gpg.conf")).unwrap();
        assert!(conf.contains("lock-never"), "{conf}");
        assert!(conf.contains("no-auto-check-trustdb"), "{conf}");
    }

    /// One build's overrides go to a file of that build's own, which
    /// `makechrootpkg` then installs into the chroot copy it makes. Nothing is
    /// written to the shared base chroot, where two builds starting together
    /// used to overwrite each other's.
    #[test]
    fn a_dropin_is_staged_per_build() {
        let tmp = tempfile::tempdir().unwrap();
        let staged = tmp.path().join("makepkg.conf");
        std::fs::write(&staged, "MAKEFLAGS=-j4\nPKGDEST=/output\nPACKAGER='a'\n").unwrap();

        let dropin = stage_makepkg_dropin(&staged).unwrap();

        assert_ne!(dropin, staged, "the drop-in is its own file");
        let text = std::fs::read_to_string(&dropin).unwrap();
        assert_eq!(text, "MAKEFLAGS=-j4\nPACKAGER='a'\n");
    }

    /// The variable has to be spelled the same here and in
    /// `packaging/patch-makechrootpkg.py`, which reads `$AURCACHE_DROPIN`:
    /// sudo drops everything not named, and a dropped drop-in is a build that
    /// silently runs on the chroot's own defaults.
    #[test]
    fn the_build_environment_survives_sudo() {
        let cmd = devtools("makechrootpkg");
        let preserve = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .find(|a| a.starts_with("--preserve-env="))
            .expect("devtools commands must preserve an environment through sudo");

        for var in ["GNUPGHOME", "SRCDEST", "AURCACHE_DROPIN"] {
            assert!(preserve.contains(var), "{preserve} must carry {var}");
        }
    }

    /// A build's devtools run gets its `TMPDIR` on the command line -- the one
    /// way through sudo -- and still the preserved environment.
    #[test]
    fn a_builds_tmpdir_reaches_devtools_through_env() {
        let cmd = devtools_in("makechrootpkg", Path::new("/pool/job-7.data/tmp"));
        let args: Vec<_> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(args[0].starts_with("--preserve-env="), "{args:?}");
        assert_eq!(
            &args[1..],
            ["env", "TMPDIR=/pool/job-7.data/tmp", "makechrootpkg"]
        );
    }

    /// `makechrootpkg` picks the build directories, and a drop-in must not
    /// override them: it bind-mounts `/pkgdest` and friends, and `/output` --
    /// which the server sends as a default -- exists nowhere in the chroot.
    #[test]
    fn the_chroots_own_directories_are_left_alone() {
        let filtered = without_chroot_owned(
            "PKGDEST=/output\nMAKEFLAGS=-j4\nSRCDEST=/srcdest\nPACKAGER='x'\nBUILDDIR=/b\n",
        );
        assert_eq!(filtered, "MAKEFLAGS=-j4\nPACKAGER='x'\n");
    }

    /// Only exact assignments go; a comment mentioning one stays, and so does
    /// anything whose name merely starts the same way.
    #[test]
    fn filtering_matches_the_setting_not_the_text() {
        let filtered = without_chroot_owned(
            "# PKGDEST is chosen by makechrootpkg\nPKGDEST_EXTRA=1\nPKGDEST=/output\n",
        );
        assert_eq!(
            filtered,
            "# PKGDEST is chosen by makechrootpkg\nPKGDEST_EXTRA=1\n"
        );
    }

    /// The staged makepkg file is the job's overrides verbatim.
    ///
    /// Nothing from the host is mixed in: that is the point of the drop-in, and
    /// a merged file here would mean the chroot's own defaults had been replaced
    /// by whatever the build machine happens to be configured for.
    ///
    /// Installing it into the chroot needs root and is left to the real build.
    #[test]
    fn only_the_jobs_overrides_are_staged() {
        let dir = tempfile::tempdir().unwrap();
        let StagedConfigs {
            makepkg_overrides: makepkg,
            ..
        } = write_configs(
            dir.path(),
            "PKGDEST=/output\nMAKEFLAGS=-j4\n",
            "[options]\n",
            None,
            None,
        )
        .unwrap();

        let staged = std::fs::read_to_string(&makepkg).unwrap();
        assert_eq!(staged, "PKGDEST=/output\nMAKEFLAGS=-j4\n");
        assert!(
            !staged.contains("CARCH") && !staged.contains("BUILDENV"),
            "the host's defaults must not be merged in: {staged}"
        );
    }
}
