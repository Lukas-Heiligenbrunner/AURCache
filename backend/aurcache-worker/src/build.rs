//! Per-package build execution inside a `devtools` chroot.
//!
//! The heavy lifting (mkarchroot / makechrootpkg) shells out to Arch's
//! `devtools`; those steps only run on a real worker image. Command
//! construction, the one pure part, is unit tested here. Source extraction,
//! artifact discovery and exit classification are executor-independent and
//! live in `aurcache_worker_core`.

use std::path::{Path, PathBuf};

use anyhow::Context;

/// The worker's build script: AURCache's derivation of devtools'
/// `makechrootpkg`, which the native package writes here.
///
/// An absolute path, not a name on `PATH`. The worker runs it through `sudo`,
/// and sudo replaces `PATH` with its own `secure_path`, so a `PATH` set for the
/// service never reaches the command. Naming the file is the only way to be
/// sure which one runs.
///
/// Not configurable, and not named `makechrootpkg`, so that nothing invites
/// pointing it at the packaged `/usr/bin/makechrootpkg`. That one executes a
/// PKGBUILD twice outside the chroot unconfined, and this copy wraps both in
/// `aurcache-sandbox` (see `packaging/patch-makechrootpkg.py`). Running the
/// stock script would be a silent loss of isolation rather than a failure,
/// which is also why [`INTERFACE`] exists.
pub const SCRIPT: &str = "/usr/lib/aurcache/bin/aurcache-build";

/// The interface [`SCRIPT`] speaks, which stock `makechrootpkg` does not.
///
/// Every build passes `--interface=<this>` first, so the stock script given
/// the same argv rejects the option and exits before it does anything, and
/// `--interface` alone makes the script print the number ([`check_script`]).
/// Must equal `INTERFACE` in `packaging/patch-makechrootpkg.py`; bump both
/// when the worker starts relying on something a copy derived by the previous
/// patcher would not do.
pub const INTERFACE: &str = "3";

/// Refuse to start unless [`SCRIPT`] is there and speaks [`INTERFACE`].
///
/// At startup rather than at the first build: a missing or stock copy would
/// otherwise surface as every build failing, long after the operator who could
/// fix it has stopped watching. The query runs before the script needs root,
/// so it is run directly rather than through `sudo`.
pub async fn check_script() -> anyhow::Result<()> {
    let output = tokio::process::Command::new(SCRIPT)
        .arg("--interface")
        .output()
        .await
        .with_context(|| {
            format!(
                "cannot run {SCRIPT}; it is derived from devtools' makechrootpkg when \
                 the aurcache-worker package is installed, and removed if devtools \
                 changed in a way the patch no longer applies to. Reinstall the \
                 package to re-derive it."
            )
        })?;
    let answer = String::from_utf8_lossy(&output.stdout);
    if output.status.success() && answer.trim() == INTERFACE {
        return Ok(());
    }
    anyhow::bail!(
        "{SCRIPT} does not speak build interface {INTERFACE} (exit {}, answered {:?}); \
         it is not a copy derived by this worker's package. Reinstall the \
         aurcache-worker package to re-derive it.",
        output.status,
        answer.trim(),
    )
}

/// A directory of the worker's mounted into a build's chroot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BindMount {
    /// Where it is on the worker.
    pub host: PathBuf,
    /// Where the build sees it.
    pub chroot: PathBuf,
    /// Whether the build can change what it sees there.
    pub access: Access,
}

/// What a build can do with a [`BindMount`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    /// Read it only: `makechrootpkg -D`, systemd-nspawn's `--bind-ro=`.
    ReadOnly,
    /// Read and write it: `makechrootpkg -d`, systemd-nspawn's `--bind=`.
    ReadWrite,
}

impl Access {
    /// The `makechrootpkg` option that binds a directory this way.
    #[must_use]
    pub const fn flag(self) -> &'static str {
        match self {
            Self::ReadOnly => "-D",
            Self::ReadWrite => "-d",
        }
    }
}

/// Build the [`SCRIPT`] argv for a per-package build.
///
/// No `systemd-run` wrapper: the worker places the build in a cgroup of its own
/// (see `crate::cgroup`), which is where its limits are set, its memory is
/// measured and a timeout or a Stop kills it.
///
/// `makechrootpkg` copies the built packages into its **current working
/// directory**, so the caller sets `cwd` to the desired destination (there is
/// no `--pkgdest` flag on `makepkg`). After the `--` separator come the flags
/// every build takes and then the job's `build_flags`, for `makepkg`.
///
/// Builds run as `build_user`, which must not be the user the worker itself
/// runs as: file ownership is what keeps the worker's identity and credentials
/// out of reach of the code a PKGBUILD executes.
///
/// `binds` become `-d host:chroot` arguments, or `-D host:chroot` for a
/// read-only one. devtools passes systemd-nspawn these *after* its own binds,
/// the read-only ones ahead of the read-write ones, so a bind here overrides
/// one devtools made for the same target — which is how the per-job pacman
/// cache replaces the shared one. There is no supported flag for that;
/// `SRCDEST`, which does have one, uses it instead.
pub fn build_command(
    chroot_root: &Path,
    copy_label: &str,
    binds: &[BindMount],
    build_flags: &[String],
    build_user: &str,
) -> Vec<String> {
    // No `-c`: the worker has already made this build's chroot, a snapshot in
    // the storage pool with a disk quota on it, and devtools finds it there.
    // Nor `-T`: the worker deletes it too, with the quota group around it.
    let mut argv = vec![SCRIPT.to_string(), format!("--interface={INTERFACE}")];
    argv.extend([
        "-r".to_string(),
        chroot_root.display().to_string(),
        "-l".to_string(),
        copy_label.to_string(),
    ]);
    // Name the build user explicitly. Without `-U`, makechrootpkg infers it
    // from `SUDO_USER`, which is whoever invoked us -- so the worker's own user
    // would run the build, and a build could then read the worker's mTLS
    // identity and credentials by ordinary file permissions.
    argv.extend(["-U".to_string(), build_user.to_string()]);
    // No `-u`, whose `pacman -Syuu` upgrades with every repository the copy
    // lists, AURCache's included -- and an upgrade is where pacman acts on a
    // package's `replaces=`. [`SCRIPT`] syncs and upgrades this build's copy
    // itself, from the configurations the caller names in its environment
    // (`crate::devtools::PacmanConfs`), and refuses `-u`.
    //
    // Per copy, before the build starts, because the shared base is only
    // refreshed on an interval (`WORKER_CHROOT_REFRESH_INTERVAL`, 15 minutes by
    // default). That is right for Arch's repositories -- they move a few times
    // a day -- and wrong for AURCache's own, which moves whenever a build here
    // finishes: a dependency this very server built two minutes ago is in the
    // repository and absent from the base chroot's sync databases, so the
    // build that was unblocked by it could not see the thing that unblocked it.
    //
    // Per copy rather than by refreshing the base is what keeps it cheap: the
    // copy is this build's alone, so nothing is locked, nothing is serialised
    // behind it, and concurrent builds each do their own. The interval refresh
    // still earns its keep -- it is what leaves this one with nothing to
    // download on the large majority of builds.

    // `SRCDEST` is passed through the environment instead of a bind mount:
    // devtools reads it directly and binds it itself, which avoids competing
    // with its own `--bind=$SRCDEST:/srcdest`.
    for BindMount {
        host,
        chroot,
        access,
    } in binds
    {
        argv.push(access.flag().to_string());
        argv.push(format!("{}:{}", host.display(), chroot.display()));
    }
    // Separator, then makepkg's arguments: the ones every build takes, then
    // the job's.
    argv.push("--".to_string());
    argv.extend(aurcache_common::build_flags::for_makepkg(build_flags));
    argv
}

/// Set a build's parallelism in its makepkg drop-in: a job per core, or per
/// CPU its limit allows.
///
/// The job's `makepkg_conf` leaves `MAKEFLAGS` to the worker, which is the
/// side that knows the limit. `cpu.max` bounds the time a build gets, not how
/// many processes it starts, and nothing inside the build can see the limit:
/// `nproc` counts the cores the machine has, and the container hides the
/// cgroup the limit is set on from anything that would look. So
/// `MAKEFLAGS=-j$(nproc)` under a limit starts a compiler per core -- 24 of
/// them sharing, say, six cores' worth of time, each holding its own memory,
/// which is how a CPU limit turns into an out-of-memory kill.
///
/// Appended, so it wins over any `MAKEFLAGS` the operator's configuration
/// set. `OMP_NUM_THREADS` is what `nproc` itself, and OpenMP, defer to;
/// `CARGO_BUILD_JOBS` is cargo's own. A build that picks its parallelism some
/// other way is still held to the limit, only less efficiently.
#[must_use]
pub fn with_parallelism(makepkg_conf: &str, cpus: Option<f64>) -> String {
    let mut out = makepkg_conf.to_string();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    let Some(cpus) = cpus else {
        out.push_str("MAKEFLAGS=\"-j$(nproc)\"\n");
        return out;
    };
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let jobs = (cpus.ceil() as u64).max(1);
    out.push_str(&format!(
        "# Added by aurcache-worker: this build is limited to {cpus} CPUs \
         (WORKER_BUILD_CPUS), so it runs as many jobs rather than one per core.\n\
         MAKEFLAGS=\"-j{jobs}\"\n\
         export OMP_NUM_THREADS={jobs}\n\
         export CARGO_BUILD_JOBS={jobs}\n"
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parallelism_follows_the_cpu_limit() {
        let server = "OPTIONS+=(!debug)\nMAKEFLAGS=-j2\nPACKAGER='x'";
        let unlimited = with_parallelism(server, None);
        assert!(unlimited.ends_with("PACKAGER='x'\nMAKEFLAGS=\"-j$(nproc)\"\n"));

        let limited = with_parallelism(server, Some(2.5));
        // After the operator's own MAKEFLAGS, so it is the one that counts.
        let ours = limited
            .find("MAKEFLAGS=\"-j3\"")
            .expect("a MAKEFLAGS for the limit");
        assert!(ours > limited.find("MAKEFLAGS=-j2").unwrap());
        assert!(limited.contains("export OMP_NUM_THREADS=3\n"));
        assert!(limited.contains("export CARGO_BUILD_JOBS=3\n"));
        assert!(
            limited.contains("PACKAGER='x'\n#"),
            "the line before is kept whole"
        );

        assert!(with_parallelism("", Some(0.5)).contains("MAKEFLAGS=\"-j1\""));
    }

    /// The worker and the patcher agree on the interface. A worker built with
    /// a bump the patcher lacks would refuse to start against every copy.
    #[test]
    fn the_patcher_writes_the_interface_the_worker_speaks() {
        let patcher = include_str!("../../../packaging/patch-makechrootpkg.py");
        assert!(
            patcher.contains(&format!("\nINTERFACE = \"{INTERFACE}\"\n")),
            "packaging/patch-makechrootpkg.py must set INTERFACE = \"{INTERFACE}\""
        );
    }

    #[test]
    fn build_command_includes_flags() {
        let cmd = build_command(
            Path::new("/chroot"),
            "job-42",
            &[],
            &["--nocheck".to_string()],
            "builder",
        );
        let joined = cmd.join(" ");
        // An absolute path, not a bare name: the command goes through `sudo`,
        // which replaces PATH with its own `secure_path`, so a name would
        // resolve to whatever sudo finds.
        assert!(
            cmd[0].starts_with('/'),
            "named by absolute path, got {}",
            cmd[0]
        );
        assert!(!cmd.iter().any(|a| a == "systemd-run"));
        assert!(!cmd.iter().any(|a| a == "--pkgdest"));
        assert!(joined.contains(&format!(
            "aurcache-build --interface={INTERFACE} -r /chroot -l job-42"
        )));
        // Builds must never inherit the worker's user via SUDO_USER.
        assert!(joined.contains("-U builder"));
        assert!(joined.ends_with("-- --noconfirm --noprogressbar --nocolor --nocheck"));
    }

    /// A server on protocol 1 sends the flags every build takes as well;
    /// makepkg gets each once.
    #[test]
    fn build_command_gives_every_build_flag_once() {
        let flags = ["--noconfirm", "--noprogressbar", "--nocolor", "--nodeps"].map(String::from);
        let cmd = build_command(Path::new("/chroot"), "job-42", &[], &flags, "builder");
        let separator = cmd.iter().position(|a| a == "--").expect("a separator");
        assert_eq!(
            cmd[separator + 1..],
            ["--noconfirm", "--noprogressbar", "--nocolor", "--nodeps"]
        );
        let bare = build_command(Path::new("/chroot"), "job-42", &[], &[], "builder");
        assert!(
            bare.join(" ")
                .ends_with("-- --noconfirm --noprogressbar --nocolor")
        );
    }

    /// `-u` upgrades with every repository the copy lists, AURCache's
    /// included, and an upgrade acts on the `replaces=` of what it can see
    /// (see `devtools::PacmanConfs`). The script syncs and upgrades the copy
    /// itself, from configurations passed in its environment, so no `-u` --
    /// alone or in a bundle -- reaches it, whatever the build flags say.
    #[test]
    fn no_build_asks_makechrootpkg_to_upgrade() {
        let flags = ["--noconfirm".to_string(), "--syncdeps".to_string()];
        let cmd = build_command(Path::new("/chroot"), "job-1", &[], &flags, "builder");
        let separator = cmd.iter().position(|a| a == "--").unwrap();
        for arg in &cmd[1..separator] {
            let short = arg.strip_prefix('-').filter(|rest| !rest.starts_with('-'));
            assert!(
                !short.is_some_and(|letters| letters.contains('u')),
                "{}",
                cmd.join(" ")
            );
        }
    }

    /// The worker makes and deletes each build's chroot, so devtools must do
    /// neither: `-c` would copy the base over the snapshot (and its quota),
    /// and `-T` would delete what devtools did not create.
    #[test]
    fn devtools_neither_makes_nor_deletes_the_chroot() {
        let cmd = build_command(Path::new("/chroot"), "job-9", &[], &[], "builder");
        let joined = cmd.join(" ");
        assert!(!cmd.iter().any(|a| a == "-c"), "{joined}");
        assert!(!cmd.iter().any(|a| a == "-T"), "{joined}");
        assert!(joined.contains("-r /chroot -l job-9"), "{joined}");
        assert!(joined.contains("-U builder"), "{joined}");
    }

    /// `SRCDEST` travels in the environment, not as a bind: devtools binds it
    /// itself, so adding one here would compete with devtools' own bind.
    #[test]
    fn build_command_does_not_bind_srcdest() {
        let cmd = build_command(Path::new("/chroot"), "job-1", &[], &[], "builder");
        assert!(!cmd.join(" ").contains("/srcdest"));
        assert!(!cmd.iter().any(|a| a == "-d" || a == "-D"));
    }

    /// Each bind is passed with the option for its access: `-d` binds
    /// read-write, `-D` read-only.
    #[test]
    fn build_command_adds_bind_mounts() {
        let binds = vec![
            BindMount {
                host: PathBuf::from("/job/cache"),
                chroot: PathBuf::from("/var/cache/job"),
                access: Access::ReadWrite,
            },
            BindMount {
                host: PathBuf::from("/host/netrc"),
                chroot: PathBuf::from("/etc/netrc"),
                access: Access::ReadOnly,
            },
        ];
        let cmd = build_command(Path::new("/chroot"), "job-7", &binds, &[], "builder");
        let joined = cmd.join(" ");
        assert!(joined.contains("-d /job/cache:/var/cache/job"), "{joined}");
        assert!(joined.contains("-D /host/netrc:/etc/netrc"), "{joined}");
        // Bind mounts are makechrootpkg's, so they come before makepkg's.
        let separator = cmd.iter().position(|a| a == "--").expect("a separator");
        assert!(
            cmd[..separator]
                .iter()
                .any(|a| a == "/host/netrc:/etc/netrc"),
            "{joined}"
        );
    }
}
