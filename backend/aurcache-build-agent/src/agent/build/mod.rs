//! The build: the root step, the drop to the build user, the pacman helper,
//! makepkg, and the report.
//!
//! `container-workers.md` ("`build`, step by step") is what this does. The
//! order is what makes it safe:
//!
//! 1. as root, before the build user has any process: take the work
//!    directory, write pacman's configuration, upgrade the image, make the
//!    build user and its directories, open the helper's socket;
//! 2. as the build user, through [`user::BuildUser::command`] and nothing
//!    else: unpack the source, fetch the keys, hold the credential, run
//!    makepkg, which reaches root only through [`helper`];
//! 3. as root again, once every other process is dead: measure, write the
//!    report, hand the directory back.

mod children;
mod credential;
mod dirs;
mod handback;
mod helper;
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
use aurcache_build_agent::makepkg_conf::{self, Rendering, Ssh};
use aurcache_build_agent::pacman_conf;
use aurcache_build_agent::report::{BuildReport, Ended, SetupStep};
use aurcache_build_agent::spec::{BuildSpec, INTERFACE};
use children::Quiet;
use helper::Helper;
use output::Output;
use run::Keep;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Component, Path};
use std::process::{Command, ExitCode, ExitStatus};
use std::sync::Arc;
use std::time::Duration;
use user::BuildUser;

/// How long the build's processes have to die once they are killed.
const ENDING: Duration = Duration::from_secs(60);

/// What root finds programs through when it was started with nothing set.
const ROOT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/bin";

const PACMAN_CONF: &str = "/etc/pacman.conf";
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

/// `PACMAN_AUTH`'s client: send `argv` to root's helper and exit with
/// pacman's status, or 126 when the helper refused it.
#[allow(clippy::needless_pass_by_value)]
pub fn pacman_request(argv: Vec<String>) -> ExitCode {
    helper::request(&argv)
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
    let output =
        Arc::new(Output::open(log).map_err(|e| format!("opening the build's output: {e}"))?);
    children::start();

    let helper = Helper::new();
    let (spec, made) = match read_spec(work) {
        Ok(spec) => {
            let made = make(&spec, work, &output, &helper);
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

    helper.close();
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
fn make(
    spec: &BuildSpec,
    work: &Work,
    output: &Arc<Output>,
    helper: &Helper,
) -> Result<Made, Failed> {
    let root = spec.scratch.as_path();
    let scratch = Scratch::new(root);

    // As root, with no process of the build user's anywhere.
    configure_pacman(spec).map_err(at(SetupStep::Config))?;
    output.say("preparing pacman's keyring");
    init_keyring(output).map_err(at(SetupStep::Keyring))?;
    output.say("upgrading the image");
    upgrade(output).map_err(at(SetupStep::Sync))?;
    if spec.credential.is_some() {
        credential::provide(output).map_err(at(SetupStep::Credential))?;
    }
    let agent = std::env::current_exe().map_err(at(SetupStep::User))?;
    dirs::lay_out(root, &scratch, work).map_err(at(SetupStep::User))?;
    let user = BuildUser::ensure(&scratch.home()).map_err(at(SetupStep::User))?;
    helper
        .listen(&scratch.pacman_socket(), Arc::clone(output))
        .map_err(at(SetupStep::User))?;

    // From here on, as the build user.
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
    let loaded = spec
        .credential
        .as_ref()
        .map(|credential| credential::load(credential, root, &scratch, &user, output))
        .transpose()
        .map_err(at(SetupStep::Credential))?;

    let conf = scratch.makepkg_conf();
    let rendered = makepkg_conf::render(&Rendering {
        job: &spec.makepkg_conf,
        work,
        scratch: &scratch,
        cpus: spec.cpus,
        agent: &agent,
        ssh: loaded.as_ref().map(|loaded| Ssh {
            socket: &loaded.socket,
            known_hosts: loaded.known_hosts.as_deref(),
        }),
    });
    std::fs::write(&conf, rendered).map_err(at(SetupStep::Config))?;

    output.say("running makepkg");
    let status = makepkg::run(
        &makepkg::Run {
            startdir: &scratch.startdir(&spec.pkgbase),
            conf: &conf,
            socket: &scratch.pacman_socket(),
            flags: &spec.build_flags,
        },
        &user,
        output,
    )
    .map_err(|e| Failed {
        step: SetupStep::Config,
        message: format!("makepkg could not be started: {e}"),
    })?;
    Ok(Made { status, user })
}

/// Check the job's pacman configuration and write it into `/etc`.
///
/// The mirrorlist is included from `pacman.conf`, so pacman reads it as more
/// of the same file and it is held to the same rules.
fn configure_pacman(spec: &BuildSpec) -> Result<(), String> {
    pacman_conf::check(&spec.pacman_conf).map_err(|e| format!("pacman.conf: {e}"))?;
    let mirrorlist = spec
        .mirrorlist
        .as_deref()
        .filter(|list| !list.trim().is_empty());
    if let Some(list) = mirrorlist {
        pacman_conf::check(list).map_err(|e| format!("the mirrorlist: {e}"))?;
    }
    std::fs::write(PACMAN_CONF, &spec.pacman_conf)
        .map_err(|e| format!("writing {PACMAN_CONF}: {e}"))?;
    if let Some(list) = mirrorlist {
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

/// Bring the image up to date with its repositories.
fn upgrade(output: &Output) -> Result<(), String> {
    let mut pacman = Command::new("pacman");
    pacman.args(["-Syu", "--noconfirm", "--noprogressbar", "--color", "never"]);
    let status = run::logged(output, &mut pacman)
        .map_err(|e| format!("pacman could not be started: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("pacman -Syu {}", run::describe(status)))
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
