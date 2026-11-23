//! makepkg itself, as the build user.

use super::children;
use super::user::BuildUser;
use aurcache_build_agent::report::Ended;
use aurcache_common::build_deps::SRCINFO_MAX;
use aurcache_common::build_flags;
use std::io;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{Command, ExitStatus};

/// What a login shell runs: makepkg in its place, with the words after the
/// script as its arguments. Nothing of the job is in the script itself.
const SCRIPT: &str = "exec makepkg \"$@\"";

/// Where makepkg runs, and with what configuration.
pub struct Place<'a> {
    /// The directory holding the PKGBUILD.
    pub startdir: &'a Path,
    /// The `makepkg.conf` written for this build.
    pub conf: &'a Path,
}

/// makepkg as the build user, through a login shell, so that it sees the
/// environment the image's profile sets up (the paths of perl's tools among
/// it), as it does in a chroot build.
fn makepkg(place: &Place<'_>, user: &BuildUser) -> Command {
    let mut makepkg = user.command("bash");
    makepkg
        .args(["-l", "-c", SCRIPT, "makepkg", "--config"])
        .arg(place.conf)
        .current_dir(place.startdir);
    makepkg
}

/// The PKGBUILD's `.SRCINFO`, as `makepkg --printsrcinfo` prints it: sourced
/// the way the build sources it, as the build user, with what it says on
/// stderr in the build log.
pub fn srcinfo(place: &Place<'_>, user: &BuildUser) -> Result<String, String> {
    let mut makepkg = makepkg(place, user);
    makepkg.arg("--printsrcinfo");
    let printed = super::run::printed(&mut makepkg, SRCINFO_MAX)
        .map_err(|e| format!("makepkg --printsrcinfo: {e}"))?;
    if !printed.status.success() {
        return Err(format!(
            "makepkg --printsrcinfo {}",
            super::run::describe(printed.status)
        ));
    }
    String::from_utf8(printed.stdout)
        .map_err(|_| "makepkg --printsrcinfo printed something not UTF-8".to_string())
}

/// Run makepkg to its end, with the flags every build takes and the job's.
///
/// Without `-s`: the dependencies are installed before it starts, and
/// makepkg checks them itself, failing the build with its own message if one
/// is missing.
pub fn run(place: &Place<'_>, flags: &[String], user: &BuildUser) -> io::Result<ExitStatus> {
    let mut makepkg = makepkg(place, user);
    makepkg
        .args(build_flags::for_makepkg(flags))
        .stdout(super::run::log()?)
        .stderr(super::run::log()?);
    let pid = children::spawn(&mut makepkg)?;
    drop(makepkg);
    children::wait(pid)
}

/// How makepkg ended, for the report.
pub fn ended(status: ExitStatus) -> Ended {
    match (status.code(), status.signal()) {
        (Some(code), _) => Ended::Exited { code },
        (None, signal) => Ended::Signaled {
            signal: signal.unwrap_or(0),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::ended;
    use aurcache_build_agent::report::Ended;
    use std::os::unix::process::ExitStatusExt;
    use std::process::ExitStatus;

    #[test]
    fn a_status_becomes_how_the_build_ended() {
        assert_eq!(ended(ExitStatus::from_raw(0)), Ended::Exited { code: 0 });
        assert_eq!(
            ended(ExitStatus::from_raw(4 << 8)),
            Ended::Exited { code: 4 }
        );
        assert_eq!(
            ended(ExitStatus::from_raw(libc::SIGKILL)),
            Ended::Signaled {
                signal: libc::SIGKILL
            }
        );
    }
}
