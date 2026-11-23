//! The build: the root step, the drop to the build user, the dependencies,
//! makepkg, and the report.
//!
//! `container-workers.md` ("`build`, step by step") is what this does. The
//! order is what makes it safe:
//!
//! 1. as root, before the build user has any process: take the work
//!    directory, write pacman's configuration, upgrade the image with
//!    AURCache's repository out of sight, and end whatever that left
//!    running; then make the build user, its directories and `makepkg.conf`;
//! 2. as the build user, through [`user::BuildUser::command`] and nothing
//!    else: unpack the source, fetch the keys, print the PKGBUILD's
//!    `.SRCINFO`;
//! 3. as root, once every other process is dead: install the dependencies
//!    it names ([`deps`]);
//! 4. as the build user, for good: makepkg, which has nothing in the
//!    container to ask root for anything;
//! 5. as root again, once every other process is dead: measure, write the
//!    report, hand the directory back.

mod children;
mod deps;
mod dirs;
mod handback;
mod init;
mod keys;
mod makepkg;
mod measure;
mod run;
mod source;
mod user;
mod vcs;

use aurcache_build_agent::layout::{Scratch, Work};
use aurcache_build_agent::makepkg_conf::{self, Rendering};
use aurcache_build_agent::report::{BuildReport, Ended, SetupStep};
use aurcache_build_agent::spec::{BuildSpec, INTERFACE};
use children::Quiet;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Component, Path};
use std::process::{Command, ExitCode, ExitStatus};
use std::time::Duration;
use user::BuildUser;

/// How long the build's processes have to die once they are killed.
const ENDING: Duration = Duration::from_secs(60);

/// What root finds programs through when it was started with nothing set.
const ROOT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/bin";

const PACMAN_CONF: &str = "/etc/pacman.conf";
/// The configuration the image is upgraded with: the job's own, without the
/// AURCache repository.
const UPGRADE_CONF: &str = "/etc/aurcache/pacman-upgrade.conf";
const MIRRORLIST: &str = "/etc/pacman.d/mirrorlist";

/// A step before makepkg that failed.
struct Failed {
    step: SetupStep,
    message: String,
}

/// Name the step a failure belongs to.
fn at<E: ToString>(step: SetupStep) -> impl Fn(E) -> Failed {
    move |error| Failed {
        step,
        message: error.to_string(),
    }
}

/// A build whose makepkg ran.
struct Made {
    status: ExitStatus,
    user: BuildUser,
}

/// Run the build in `work`, as PID 1 and root. Exits 0 if and only if it
/// wrote `<work>/out/report.json`.
pub fn run(work: &Path) -> ExitCode {
    match build(&Work::new(work)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            eprintln!("aurcache-build-agent: {why}");
            ExitCode::FAILURE
        }
    }
}

/// The whole build. An error is a build that leaves no report: the agent
/// could not make sure of the place it writes one to.
fn build(work: &Work) -> Result<(), String> {
    init::placed()?;
    init::no_fault_handlers();
    init::undumpable().map_err(|e| format!("making the agent undumpable: {e}"))?;
    // SAFETY: the agent has no other thread, so nothing reads the environment
    // or creates a file while these change.
    unsafe {
        libc::umask(0o022);
        if std::env::var_os("PATH").is_none() {
            std::env::set_var("PATH", ROOT_PATH);
        }
    }
    let owner = dirs::take(work).map_err(|e| format!("taking {}: {e}", work.root().display()))?;

    let spec = read_spec(work);
    let made = match &spec {
        Ok(spec) => make(spec, work),
        Err(message) => Err(Failed {
            step: SetupStep::Spec,
            message: message.clone(),
        }),
    };
    clear("the build")?;
    let vcs_commits = if let Ok(spec) = &spec
        && let Ok(made) = &made
    {
        let commits = vcs::commits(&spec.vcs_sources, &Scratch::new(&spec.scratch), &made.user);
        clear("git")?;
        commits
    } else {
        BTreeMap::new()
    };
    let ended = match made {
        Ok(made) => makepkg::ended(made.status),
        Err(Failed { step, message }) => {
            run::say(&format!("the build could not start: {message}"));
            Ended::Setup { step, message }
        }
    };

    let memory = measure::memory();
    let report = BuildReport {
        interface: INTERFACE,
        ended,
        peak_memory_bytes: memory.peak_bytes,
        oom_kills: memory.oom_kills,
        disk_bytes: spec
            .as_ref()
            .ok()
            .and_then(|spec| measure::disk(work.root(), &spec.scratch)),
        vcs_commits,
    };
    handback::write(work, &report).map_err(|e| format!("writing the report: {e}"))?;
    handback::give(work, owner);
    Ok(())
}

/// Kill every other process in the container: whatever `what` left
/// running. Each step that follows is root's or the build user's alone, with
/// nothing of the other's beside it, and the report is written once nothing
/// the build started can change what it reads.
fn clear(what: &str) -> Result<(), String> {
    match children::kill_all(ENDING) {
        Quiet::Alone => Ok(()),
        Quiet::Lingering => Err(format!("a process {what} started outlived being killed")),
    }
}

/// Read the job, refusing one written for another interface.
fn read_spec(work: &Work) -> Result<BuildSpec, String> {
    /// The one field every interface has.
    #[derive(Deserialize)]
    struct Numbered {
        interface: u32,
    }
    let path = work.spec();
    let bytes = std::fs::read(&path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    let numbered: Numbered = serde_json::from_slice(&bytes)
        .map_err(|e| format!("{} is not a job: {e}", path.display()))?;
    if numbered.interface != INTERFACE {
        return Err(format!(
            "the job was written for interface {} and this agent speaks interface {INTERFACE}: \
             the worker and its build agent are not the same version",
            numbered.interface
        ));
    }
    let spec: BuildSpec = serde_json::from_slice(&bytes)
        .map_err(|e| format!("{} is not a job: {e}", path.display()))?;
    if !is_one_name(&spec.pkgbase) {
        return Err(format!("`{}` is not a package base", spec.pkgbase));
    }
    if !spec.scratch.is_absolute() {
        return Err(format!(
            "the scratch directory `{}` is not an absolute path",
            spec.scratch.display()
        ));
    }
    Ok(spec)
}

/// Whether `name` is one path component, as a package base and a VCS
/// mirror's directory under `SRCDEST` must be.
fn is_one_name(name: &str) -> bool {
    let mut parts = Path::new(name).components();
    matches!(parts.next(), Some(Component::Normal(_))) && parts.next().is_none()
}

/// Everything from the spec to makepkg's end.
fn make(spec: &BuildSpec, work: &Work) -> Result<Made, Failed> {
    let root = spec.scratch.as_path();
    let scratch = Scratch::new(root);

    // As root, with no process of the build user's anywhere.
    configure_pacman(spec).map_err(at(SetupStep::Config))?;
    run::say("preparing pacman's keyring");
    init_keyring().map_err(at(SetupStep::Keyring))?;
    run::say("upgrading the image");
    upgrade().map_err(at(SetupStep::Sync))?;
    // pacman-key's gpg-agent, for one, outlives the root step.
    clear("the root step").map_err(at(SetupStep::Sync))?;
    dirs::lay_out(root, &scratch, work).map_err(at(SetupStep::User))?;
    let user = BuildUser::ensure(&scratch.home()).map_err(at(SetupStep::User))?;
    let conf = scratch.makepkg_conf();
    let rendered = makepkg_conf::render(&Rendering {
        job: &spec.makepkg_conf,
        work,
        scratch: &scratch,
        cpus: spec.cpus,
    });
    std::fs::write(&conf, rendered).map_err(at(SetupStep::Config))?;

    // As the build user, but for the dependencies' install.
    source::unpack(work, root, &scratch, &spec.pkgbase, &user).map_err(at(SetupStep::Source))?;
    if !spec.pgp_keys.is_empty() {
        run::say("fetching the package's PGP keys");
        keys::import(&spec.pgp_keys, &spec.keyserver, &user).map_err(at(SetupStep::Keys))?;
    }
    let place = makepkg::Place {
        startdir: &scratch.startdir(&spec.pkgbase),
        conf: &conf,
    };
    let needs = deps::Deps {
        place: &place,
        arch: &spec.arch,
        flags: &spec.build_flags,
    };
    deps::install(&needs, &user).map_err(at(SetupStep::Dependencies))?;

    run::say("running makepkg");
    let status = makepkg::run(&place, &spec.build_flags, &user).map_err(at(SetupStep::Makepkg))?;
    Ok(Made { status, user })
}

/// Write the job's pacman configuration into `/etc`: the whole of it as
/// `pacman.conf`, and beside it the one the image is upgraded with.
fn configure_pacman(spec: &BuildSpec) -> Result<(), String> {
    std::fs::write(PACMAN_CONF, &spec.pacman_conf)
        .map_err(|e| format!("writing {PACMAN_CONF}: {e}"))?;
    if let Some(dir) = Path::new(UPGRADE_CONF).parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("making {}: {e}", dir.display()))?;
    }
    std::fs::write(UPGRADE_CONF, &spec.pacman_upgrade_conf)
        .map_err(|e| format!("writing {UPGRADE_CONF}: {e}"))?;
    if let Some(list) = spec.mirrorlist.as_deref().filter(|l| !l.trim().is_empty()) {
        std::fs::write(MIRRORLIST, list).map_err(|e| format!("writing {MIRRORLIST}: {e}"))?;
    }
    Ok(())
}

/// Give this container's pacman a keyring master key of its own. Without
/// one, an upgrade of the keyring package imports nothing.
fn init_keyring() -> Result<(), String> {
    let mut pacman_key = Command::new("pacman-key");
    pacman_key.arg("--init");
    let status = run::logged(&mut pacman_key)
        .map_err(|e| format!("pacman-key could not be started: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("pacman-key --init {}", run::describe(status)))
    }
}

/// Bring the image up to date with its repositories, AURCache's left out.
///
/// The databases are synced once, with every repository the build will
/// install from, so the upgrade and the dependencies are of one state. The
/// upgrade itself sees only the job's own repositories: pacman acts on
/// `replaces=` during an upgrade, and a package AURCache built must not get
/// itself installed that way.
fn upgrade() -> Result<(), String> {
    pacman(&["-Sy"], "pacman -Sy")?;
    pacman(&["-Su", "--config", UPGRADE_CONF], "pacman -Su")
}

/// Run pacman as root with what every run here has, then `args`, which may
/// end in `--` and targets.
fn pacman(args: &[&str], what: &str) -> Result<(), String> {
    let mut pacman = Command::new("pacman");
    pacman
        .args(["--noconfirm", "--noprogressbar", "--color", "never"])
        .args(args);
    let status =
        run::logged(&mut pacman).map_err(|e| format!("pacman could not be started: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{what} {}", run::describe(status)))
    }
}

#[cfg(test)]
mod tests {
    use super::{is_one_name, read_spec};
    use aurcache_build_agent::layout::Work;

    const SPEC: &str = include_str!("../../../tests/fixtures/interface-1/spec.json");

    /// A job's work directory, for as long as this is held.
    struct Job {
        _dir: tempfile::TempDir,
        work: Work,
    }

    /// The work directory of a job holding this spec.
    fn job(spec: &serde_json::Value) -> Job {
        let dir = tempfile::tempdir().unwrap();
        let work = Work::new(dir.path());
        std::fs::create_dir(work.input()).unwrap();
        std::fs::write(work.spec(), serde_json::to_vec(spec).unwrap()).unwrap();
        Job { _dir: dir, work }
    }

    fn fixture() -> serde_json::Value {
        let specs: Vec<serde_json::Value> = serde_json::from_str(SPEC).unwrap();
        specs.into_iter().next().unwrap()
    }

    #[test]
    fn a_spec_of_this_interface_is_read() {
        let job = job(&fixture());
        let spec = read_spec(&job.work).unwrap();
        assert_eq!(spec.pkgbase, "hello");
        assert_eq!(spec.build_flags, ["--skippgpcheck"]);
    }

    /// Another interface is refused by its number, before anything else of
    /// it is read: its other fields may mean something else.
    #[test]
    fn another_interface_is_refused_by_its_number() {
        let other = job(&serde_json::json!({"interface": 2, "packages": []}));
        let refused = read_spec(&other.work).unwrap_err();
        assert!(refused.contains("interface 2"), "{refused}");

        let mut spec = fixture();
        spec["interface"] = 0.into();
        let zero = job(&spec);
        assert!(read_spec(&zero.work).unwrap_err().contains("interface 0"));
    }

    #[test]
    fn a_spec_that_names_no_place_is_refused() {
        for pkgbase in ["", ".", "..", "a/b", "/etc", "../x"] {
            let mut spec = fixture();
            spec["pkgbase"] = pkgbase.into();
            let job = job(&spec);
            assert!(read_spec(&job.work).is_err(), "{pkgbase:?}");
            assert!(!is_one_name(pkgbase), "{pkgbase:?}");
        }
        let mut spec = fixture();
        spec["scratch"] = "relative/scratch".into();
        let job = job(&spec);
        assert!(read_spec(&job.work).unwrap_err().contains("absolute"));

        let tmp = tempfile::tempdir().unwrap();
        assert!(read_spec(&Work::new(tmp.path())).is_err());
    }
}
