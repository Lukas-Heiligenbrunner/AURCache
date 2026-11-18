//! The build's dependencies, installed by root before the build starts.
//!
//! The build user prints the PKGBUILD's `.SRCINFO`, with nothing more than
//! it has for the build. Once nothing of its own is left running, root reads
//! from it what makepkg would install (`aurcache_common::build_deps`, which
//! the chroot worker reads it with too), asks pacman which of those the
//! image lacks, as makepkg does, and installs those as dependencies. Once
//! nothing that install started is left running either, the build cannot
//! reach root at all: makepkg runs without `-s`, checks its dependencies
//! itself, and fails with its own message on one that is missing.
//!
//! What the `.SRCINFO` says is the build's to choose, so it only ever names
//! targets: every word root's pacman is given before `--` is the agent's.

use super::makepkg::{self, Place};
use super::output::Output;
use super::run;
use super::user::BuildUser;
use super::{clear, pacman};
use aurcache_common::build_deps::{Install, SRCINFO_MAX, from_srcinfo};
use std::process::Command;

/// What `pacman -T` exits with when some of what it was asked about is
/// missing; it prints those, one per line.
const SOME_MISSING: i32 = 127;

/// What deciding a build's dependencies needs.
pub struct Deps<'a> {
    pub place: &'a Place<'a>,
    /// The architecture the job is built for.
    pub arch: &'a str,
    /// The job's makepkg flags, which say whether any are installed and
    /// whether `checkdepends` are.
    pub flags: &'a [String],
}

/// Install what the build depends on and the image lacks.
pub fn install(deps: &Deps<'_>, user: &BuildUser, output: &Output) -> Result<(), String> {
    let checks = match Install::of(deps.flags) {
        Install::Nothing => return Ok(()),
        Install::Dependencies(checks) => checks,
    };
    output.say("reading the package's dependencies");
    let srcinfo = makepkg::srcinfo(deps.place, user, output)?;
    // Root's pacman, and the scriptlets it runs, run with no process of the
    // build user's beside them. What it wrote stays: the threat model in
    // design/in-progress/container-workers.md says why that is minor.
    clear("the PKGBUILD")?;
    let wanted = from_srcinfo(&srcinfo, deps.arch, checks);
    let lacking = missing(&wanted, output)?;
    if lacking.is_empty() {
        return Ok(());
    }
    output.say(&format!("installing {}", lacking.join(" ")));
    let mut args = vec!["-S", "--asdeps", "--needed", "--"];
    args.extend(lacking.iter().map(String::as_str));
    pacman(output, &args, "pacman -S --asdeps")?;
    // And the build runs with nothing of root's beside it: a scriptlet may
    // have left a process running.
    clear("an install scriptlet")
}

/// Which of `deps` no installed package satisfies, as `pacman -T` says.
/// It prints no more than it was given, which is no more than a `.SRCINFO`.
fn missing(deps: &[String], output: &Output) -> Result<Vec<String>, String> {
    if deps.is_empty() {
        return Ok(Vec::new());
    }
    let mut pacman = Command::new("pacman");
    pacman.args(["-T", "--"]).args(deps);
    let ran =
        run::printed(output, &mut pacman, SRCINFO_MAX).map_err(|e| format!("pacman -T: {e}"))?;
    match ran.status.code() {
        Some(0) => Ok(Vec::new()),
        Some(SOME_MISSING) => Ok(String::from_utf8_lossy(&ran.stdout)
            .lines()
            .map(str::to_string)
            .collect()),
        _ => Err(format!("pacman -T {}", run::describe(ran.status))),
    }
}
