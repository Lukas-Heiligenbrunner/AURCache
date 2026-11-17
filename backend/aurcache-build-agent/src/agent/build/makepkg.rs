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
    /// The `makepkg.conf` written for this run.
    pub conf: &'a Path,
    /// The pacman helper's socket, for `PACMAN_AUTH`'s client.
    pub socket: &'a Path,
    /// What makepkg is told after `--config`: [`download`] or [`build`].
    pub args: &'a [String],
}

/// Whether the sources are in `SRCDEST` already.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sources {
    /// A download before the build fetched them.
    Fetched,
    /// The build fetches them itself.
    ToFetch,
}

/// The arguments of a run that downloads the sources and does nothing else:
/// no dependency installed, no function of the PKGBUILD run, nothing
/// checked (the build checks them). The job's `--ignorearch` is kept, as
/// without it makepkg stops at the architecture before downloading.
#[must_use]
pub fn download(flags: &[String]) -> Vec<String> {
    let mut args = vec!["--verifysource".to_string(), "--skipinteg".to_string()];
    if flags.iter().any(|flag| ignores_arch(flag)) {
        args.push("--ignorearch".to_string());
    }
    args
}

/// The arguments of the build: `-s` and the job's flags, with `--holdver`
/// after a download so that the VCS clones it made are built as they are.
#[must_use]
pub fn build(flags: &[String], sources: Sources) -> Vec<String> {
    let mut args = vec!["-s".to_string()];
    if sources == Sources::Fetched {
        args.push("--holdver".to_string());
    }
    args.extend(flags.iter().cloned());
    args
}

/// Whether a flag of the job's is makepkg's `-A`, alone or among other short
/// options.
fn ignores_arch(flag: &str) -> bool {
    flag == "--ignorearch"
        || flag
            .strip_prefix('-')
            .is_some_and(|short| !short.starts_with('-') && short.contains('A'))
}

/// Run makepkg to its end.
///
/// Through a login shell, so that the build sees the environment the image's
/// profile sets up (the paths of perl's tools among it), as it does in a
/// chroot build. With `-s` makepkg installs what is missing, which it does
/// through `PACMAN_AUTH`, and so through the helper.
pub fn run(run: &Run<'_>, user: &BuildUser, output: &Output) -> io::Result<ExitStatus> {
    let mut makepkg = user.command("bash");
    makepkg
        .args(["-l", "-c", SCRIPT, "makepkg", "--config"])
        .arg(run.conf)
        .args(run.args)
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
    use super::{Sources, build, download, ended};

    fn words(line: &[&str]) -> Vec<String> {
        line.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn a_download_is_only_that() {
        assert_eq!(download(&[]), ["--verifysource", "--skipinteg"]);
        // The job's flags stay out of it, `-s` above all.
        assert_eq!(
            download(&words(&["-s", "--skippgpcheck", "--nocheck"])),
            ["--verifysource", "--skipinteg"]
        );
        for arch in ["-A", "--ignorearch", "-fA", "-Acs"] {
            assert_eq!(
                download(&words(&[arch])),
                ["--verifysource", "--skipinteg", "--ignorearch"],
                "{arch}"
            );
        }
        assert_eq!(download(&words(&["--Archive"])).len(), 2);
    }

    #[test]
    fn a_build_after_a_download_holds_the_vcs_clones() {
        let flags = words(&["--skippgpcheck"]);
        assert_eq!(build(&flags, Sources::ToFetch), ["-s", "--skippgpcheck"]);
        assert_eq!(
            build(&flags, Sources::Fetched),
            ["-s", "--holdver", "--skippgpcheck"]
        );
    }

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
