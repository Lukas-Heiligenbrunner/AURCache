//! The build: the root step, the drop to the build user, the dependencies,
//! makepkg, and the report.
//!
//! `container-workers.md` ("`build`, step by step") is what this does. The
//! order is what makes it safe:
//!
//! 1. as root, before the build user has any process: take the work
//!    directory, write pacman's and makepkg's configuration, upgrade the
//!    image with AURCache's repository out of sight, make the build user and
//!    its directories; then end whatever that left running;
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
mod output;
mod run;
mod source;
mod user;
mod vcs;

use aurcache_build_agent::layout::{Scratch, Work};
use aurcache_build_agent::makepkg_conf::{self, Rendering};
use aurcache_build_agent::pacman_conf;
use aurcache_build_agent::report::{BuildReport, Ended, SetupStep};
use aurcache_build_agent::spec::{BuildSpec, INTERFACE};
use children::Quiet;
use output::Output;
use run::Keep;
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
pub fn run(work: &Path, log_file: Option<&Path>) -> ExitCode {
    match build(&Work::new(work), log_file) {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            eprintln!("aurcache-build-agent: {why}");
            ExitCode::FAILURE
        }
    }
}

/// Extract the source archive into `into`, as the build user.
pub fn unpack(archive: &Path, into: &Path) -> ExitCode {
    source::extract(archive, into)
}

/// The whole build. An error is a build that leaves no report: the agent
/// could not make sure of the place it writes one to.
fn build(work: &Work, log_file: Option<&Path>) -> Result<(), String> {
    init::placed().map_err(|e| e.to_string())?;
    init::stop_on_signal().map_err(|e| format!("installing the stop handler: {e}"))?;
    // SAFETY: no other thread exists yet, so nothing reads the environment
    // or creates a file while these change.
    unsafe {
        libc::umask(0o022);
        if std::env::var_os("PATH").is_none() {
            std::env::set_var("PATH", ROOT_PATH);
        }
    }
    let owner = dirs::take(work).map_err(|e| format!("taking {}: {e}", work.root().display()))?;
    let log = log_file
        .map(dirs::log)
        .transpose()
        .map_err(|e| format!("opening the build log: {e}"))?;
    let output = Output::open(log).map_err(|e| format!("opening the build's output: {e}"))?;
    children::start();

    let (spec, made) = match read_spec(work) {
        Ok(spec) => {
            let made = make(&spec, work, &output);
            (Some(spec), made)
        }
        Err(message) => (
            None,
            Err(Failed {
                step: SetupStep::Spec,
                message,
            }),
        ),
    };

    end()?;
    let vcs_commits = match (&spec, &made) {
        (Some(spec), Ok(made)) => {
            let commits = vcs::commits(&spec.vcs_sources, &Scratch::new(&spec.scratch), &made.user);
            end()?;
            commits
        }
        _ => BTreeMap::new(),
    };
    let ended = match made {
        Ok(made) => makepkg::ended(made.status),
        Err(Failed { step, message }) => {
            output.say(&format!("the build could not start: {message}"));
            Ended::Setup { step, message }
        }
    };
    output.close();

    let memory = measure::memory();
    let report = BuildReport {
        interface: INTERFACE,
        ended,
        peak_memory_bytes: memory.peak_bytes,
        oom_kills: memory.oom_kills,
        disk_bytes: spec
            .as_ref()
            .and_then(|spec| measure::disk(work.root(), &spec.scratch)),
        vcs_commits,
    };
    handback::write(work, &report).map_err(|e| format!("writing the report: {e}"))?;
    handback::give(work, owner);
    Ok(())
}

/// Kill everything the build left running, and make sure of it. What comes
/// next reads and gives away files that only a dead build cannot change.
fn end() -> Result<(), String> {
    match children::kill_all(ENDING) {
        Quiet::Alone => Ok(()),
        Quiet::Lingering => {
            Err("a process of the build outlived being killed; nothing is reported".to_string())
        }
    }
}

/// Kill every other process in the container: whatever `what` left
/// running. Each step that follows is root's or the build user's alone, with
/// nothing of the other's beside it.
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

/// Whether a package base can be a directory's name.
fn is_one_name(pkgbase: &str) -> bool {
    let mut parts = Path::new(pkgbase).components();
    matches!(parts.next(), Some(Component::Normal(_))) && parts.next().is_none()
}

/// Everything from the spec to makepkg's end.
fn make(spec: &BuildSpec, work: &Work, output: &Output) -> Result<Made, Failed> {
    let root = spec.scratch.as_path();
    let scratch = Scratch::new(root);

    // As root, with no process of the build user's anywhere.
    configure_pacman(spec).map_err(at(SetupStep::Config))?;
    output.say("preparing pacman's keyring");
    init_keyring(output).map_err(at(SetupStep::Keyring))?;
    output.say("upgrading the image");
    upgrade(output).map_err(at(SetupStep::Sync))?;
    let agent = std::env::current_exe().map_err(at(SetupStep::User))?;
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
    // pacman-key's gpg-agent, for one, outlives the root step.
    clear("the root step").map_err(at(SetupStep::User))?;

    // As the build user, but for the dependencies' install.
    let unpacking = source::Unpacking {
        work,
        root,
        scratch: &scratch,
        pkgbase: &spec.pkgbase,
        agent: &agent,
    };
    source::unpack(&unpacking, &user, output).map_err(at(SetupStep::Source))?;
    if !spec.pgp_keys.is_empty() {
        output.say("fetching the package's PGP keys");
        keys::import(&spec.pgp_keys, &spec.keyserver, &user, output)
            .map_err(at(SetupStep::Keys))?;
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
    deps::install(&needs, &user, output).map_err(at(SetupStep::Dependencies))?;

    output.say("running makepkg");
    let status = makepkg::run(&place, &spec.build_flags, &user, output).map_err(|e| Failed {
        step: SetupStep::Config,
        message: format!("makepkg could not be started: {e}"),
    })?;
    Ok(Made { status, user })
}

/// The job's pacman configuration, checked: what goes where in `/etc`.
#[derive(Debug, PartialEq, Eq)]
struct Confs<'a> {
    /// `/etc/pacman.conf`.
    full: &'a str,
    /// The configuration the image is upgraded with.
    upgrade: &'a str,
    mirrorlist: Option<&'a str>,
}

/// Check the job's pacman configuration.
///
/// Both configurations are held to the same rules. The mirrorlist is
/// included from either, so pacman reads it as more of the same file and it
/// is held to them too.
fn checked_confs(spec: &BuildSpec) -> Result<Confs<'_>, String> {
    pacman_conf::check(&spec.pacman_conf).map_err(|e| format!("pacman.conf: {e}"))?;
    pacman_conf::check(&spec.pacman_upgrade_conf)
        .map_err(|e| format!("the pacman.conf for the upgrade: {e}"))?;
    let mirrorlist = spec
        .mirrorlist
        .as_deref()
        .filter(|list| !list.trim().is_empty());
    if let Some(list) = mirrorlist {
        pacman_conf::check(list).map_err(|e| format!("the mirrorlist: {e}"))?;
    }
    Ok(Confs {
        full: &spec.pacman_conf,
        upgrade: &spec.pacman_upgrade_conf,
        mirrorlist,
    })
}

/// Check the job's pacman configuration and write it into `/etc`: the whole
/// of it as `pacman.conf`, and beside it the one the image is upgraded with.
fn configure_pacman(spec: &BuildSpec) -> Result<(), String> {
    let confs = checked_confs(spec)?;
    std::fs::write(PACMAN_CONF, confs.full).map_err(|e| format!("writing {PACMAN_CONF}: {e}"))?;
    if let Some(dir) = Path::new(UPGRADE_CONF).parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("making {}: {e}", dir.display()))?;
    }
    std::fs::write(UPGRADE_CONF, confs.upgrade)
        .map_err(|e| format!("writing {UPGRADE_CONF}: {e}"))?;
    if let Some(list) = confs.mirrorlist {
        std::fs::write(MIRRORLIST, list).map_err(|e| format!("writing {MIRRORLIST}: {e}"))?;
    }
    Ok(())
}

/// Give this container's pacman a keyring master key of its own. Without
/// one, an upgrade of the keyring package imports nothing. Quiet unless it
/// fails.
fn init_keyring(output: &Output) -> Result<(), String> {
    let mut pacman_key = Command::new("pacman-key");
    pacman_key.arg("--init");
    let ran = run::captured(&mut pacman_key, Keep::Both, None)
        .map_err(|e| format!("pacman-key could not be started: {e}"))?;
    if ran.succeeded() {
        return Ok(());
    }
    output.relay(&ran.text);
    Err(format!(
        "pacman-key --init {}",
        ran.status
            .map_or_else(|| "did not end".to_string(), run::describe)
    ))
}

/// Bring the image up to date with its repositories, AURCache's left out.
///
/// The databases are synced once, with every repository the build will
/// install from, so the upgrade and the build's dependencies are of the same
/// state. The upgrade itself sees only the job's own repositories: pacman
/// acts on `replaces=` during an upgrade, and a package AURCache built must
/// not get itself installed that way. Either step failing fails the build:
/// an upgrade that stopped over fresh databases is a partial one.
fn upgrade(output: &Output) -> Result<(), String> {
    for step in UPGRADE {
        pacman(output, step.args, step.what)?;
    }
    Ok(())
}

/// One pacman run of the upgrade.
struct Step {
    args: &'static [&'static str],
    /// How the run is named when it fails.
    what: &'static str,
}

/// The upgrade, in order: every database synced with `/etc/pacman.conf`,
/// then the system upgraded with the configuration that does not list
/// AURCache's repository.
const UPGRADE: [Step; 2] = [
    Step {
        args: &["-Sy"],
        what: "pacman -Sy",
    },
    Step {
        args: &["-Su", "--config", UPGRADE_CONF],
        what: "pacman -Su",
    },
];

/// Run pacman as root with what every run here has, then `args`, which may
/// end in `--` and targets.
fn pacman(output: &Output, args: &[&str], what: &str) -> Result<(), String> {
    let mut pacman = Command::new("pacman");
    pacman
        .args(["--noconfirm", "--noprogressbar", "--color", "never"])
        .args(args);
    let status = run::logged(output, &mut pacman)
        .map_err(|e| format!("pacman could not be started: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{what} {}", run::describe(status)))
    }
}

#[cfg(test)]
mod tests {
    use super::{Confs, UPGRADE, UPGRADE_CONF, checked_confs, is_one_name, read_spec};
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

    /// The image is upgraded with the job's configuration as the server
    /// sent it, and both configurations are held to the same rules.
    #[test]
    fn the_upgrade_has_a_configuration_of_its_own() {
        let job = job(&fixture());
        let spec = read_spec(&job.work).unwrap();
        let confs = checked_confs(&spec).unwrap();
        assert_eq!(
            confs,
            Confs {
                full: &spec.pacman_conf,
                upgrade: &spec.pacman_upgrade_conf,
                mirrorlist: spec.mirrorlist.as_deref(),
            }
        );
        assert!(confs.full.contains("[aurcache]"));
        assert!(!confs.upgrade.contains("[aurcache]"));
        assert!(confs.full.starts_with(confs.upgrade.trim_end()));

        let mut bad = fixture();
        bad["pacman_upgrade_conf"] = "[options]\nRootDir = /elsewhere\n".into();
        let bad = self::job(&bad);
        let refused = checked_confs(&read_spec(&bad.work).unwrap()).unwrap_err();
        assert!(refused.contains("for the upgrade"), "{refused}");
    }

    /// Every database is synced first, AURCache's included, and the system
    /// is then upgraded from the configuration without it. No run both
    /// syncs and upgrades: one that did would upgrade with the repository in
    /// view, and act on a `replaces=` in it.
    #[test]
    fn the_upgrade_never_sees_the_repository() {
        let runs: Vec<&[&str]> = UPGRADE.iter().map(|step| step.args).collect();
        assert_eq!(runs, [&["-Sy"][..], &["-Su", "--config", UPGRADE_CONF][..]]);
        for args in runs {
            let upgrades = args.iter().any(|word| {
                word.strip_prefix('-')
                    .filter(|letters| !letters.starts_with('-'))
                    .is_some_and(|letters| letters.contains('S') && letters.contains('u'))
            });
            let config = args
                .windows(2)
                .any(|pair| pair == ["--config", UPGRADE_CONF]);
            assert!(
                !upgrades || config,
                "an upgrade with the build's configuration: {args:?}"
            );
        }
    }
}
