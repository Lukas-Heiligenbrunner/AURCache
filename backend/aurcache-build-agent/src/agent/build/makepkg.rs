//! makepkg itself, as the build user.

use super::children;
use super::output::Output;
use super::user::BuildUser;
use aurcache_build_agent::layout::PACMAN_SOCKET_VAR;
use aurcache_build_agent::report::Ended;
use std::io;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::ExitStatus;

/// What a login shell runs: makepkg in its place, with the words after the
/// script as its arguments. Nothing of the job is in the script itself.
const SCRIPT: &str = "exec makepkg \"$@\"";

/// Where one makepkg run happens and what it is told.
pub struct Run<'a> {
    /// The directory holding the PKGBUILD.
    pub startdir: &'a Path,
    /// The `makepkg.conf` written for this build.
    pub conf: &'a Path,
    /// The pacman helper's socket, for `PACMAN_AUTH`'s client.
    pub socket: &'a Path,
    /// The job's own arguments.
    pub flags: &'a [String],
}

/// Run `makepkg -s` to its end.
///
/// Through a login shell, so that the build sees the environment the image's
/// profile sets up (the paths of perl's tools among it), as it does in a
/// chroot build. `-s` makes makepkg install what is missing, which it does
/// through `PACMAN_AUTH`, and so through the helper.
pub fn run(run: &Run<'_>, user: &BuildUser, output: &Output) -> io::Result<ExitStatus> {
    let mut makepkg = user.command("bash");
    makepkg
        .args(["-l", "-c", SCRIPT, "makepkg", "--config"])
        .arg(run.conf)
        .arg("-s")
        .args(run.flags)
        .current_dir(run.startdir)
        .env(PACMAN_SOCKET_VAR, run.socket)
        .stdout(output.stdio()?)
        .stderr(output.stdio()?);
    let running = children::spawn(&mut makepkg)?;
    drop(makepkg);
    Ok(children::wait(running.pid))
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
