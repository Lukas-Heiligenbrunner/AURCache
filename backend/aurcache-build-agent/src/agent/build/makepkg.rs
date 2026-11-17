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
use std::time::Duration;

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
    match run_within(run, user, output, None)? {
        Within::Ended(status) => Ok(status),
        // `None` never times out.
        Within::TimedOut => unreachable!("an unbounded run does not time out"),
    }
}

/// Whether the PKGBUILD in `startdir` has a source fetched over SSH, from
/// what `makepkg --printsrcinfo` prints, run as the build user with no
/// socket to reach. Its output is data to this, nothing more. A run that
/// fails reads as one that needs the key, so that a PKGBUILD the reader
/// trips on is not built without it.
pub fn fetches_over_ssh(startdir: &Path, conf: &Path, user: &BuildUser) -> bool {
    let mut printing = user.command("bash");
    printing
        .args(["-l", "-c", SCRIPT, "makepkg", "--config"])
        .arg(conf)
        .arg("--printsrcinfo")
        .current_dir(startdir);
    match super::run::capture(&mut printing, super::run::Keep::Stdout)
        .and_then(|capturing| capturing.finish(Some(PRINT_TIME)))
    {
        Ok(printed) if printed.succeeded() => {
            aurcache_build_agent::srcinfo::needs_ssh(&printed.text)
        }
        _ => true,
    }
}

/// How long `makepkg --printsrcinfo` may take.
const PRINT_TIME: Duration = Duration::from_secs(120);

/// How a bounded run ended.
pub enum Within {
    /// makepkg ended on its own, within the time it was given.
    Ended(ExitStatus),
    /// It ran past `limit` and was killed. What it left is cleaned up by the
    /// caller's end-of-build kill.
    TimedOut,
}

/// Run makepkg, killing it if it has not ended within `limit`.
///
/// The download that holds the SSH window open is bounded this way, so a
/// PKGBUILD's top-level `sleep` cannot keep the window open until the build
/// timeout. `None` lets it run to its own end, as the build itself does.
pub fn run_within(
    run: &Run<'_>,
    user: &BuildUser,
    output: &Output,
    limit: Option<Duration>,
) -> io::Result<Within> {
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
    match limit {
        None => Ok(Within::Ended(children::wait(running.pid))),
        Some(limit) => match children::wait_within(running.pid, limit) {
            Some(status) => Ok(Within::Ended(status)),
            None => {
                children::kill(running.pid);
                Ok(Within::TimedOut)
            }
        },
    }
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
