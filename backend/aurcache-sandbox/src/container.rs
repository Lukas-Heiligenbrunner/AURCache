//! The `container` profile: what starts a build's containers.
//!
//! ```text
//! aurcache-sandbox container -- PROGRAM [ARGS]...
//! ```
//!
//! The patched build script starts every container it makes through this,
//! so that the container and everything in it are held to [`NO_VSOCK`]:
//! no `AF_VSOCK` socket, which no network namespace confines and no network
//! policy sees, and no io_uring, which would create one without `socket`.
//! The filter is installed here, on the host, and `systemd-nspawn` and the
//! container inherit it, whatever nspawn's version and whatever ABI the
//! container's programs call through (see `seccomp.rs`).
//!
//! Run as root, as the build script is, it needs nothing else. Where no
//! filter can be installed -- an emulated worker, whose qemu refuses one --
//! the program runs anyway, after a warning on stderr: an emulated worker
//! builds, with vsock open, rather than not at all.
//!
//! [`NO_VSOCK`]: crate::seccomp::NO_VSOCK

use std::ffi::OsString;
use std::os::unix::process::CommandExt;
use std::process::ExitCode;

use crate::seccomp;

const USAGE: &str = "usage: aurcache-sandbox container -- PROGRAM [ARGS]...";

struct Request {
    program: OsString,
    args: Vec<OsString>,
}

pub fn main(args: impl Iterator<Item = OsString>) -> ExitCode {
    let request = match parse_args(args) {
        Ok(request) => request,
        Err(e) => {
            eprintln!("aurcache-sandbox container: {e}");
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    match seccomp::refuse_vsock() {
        Ok(vsock) => {
            if let Some(warning) = vsock.warning() {
                eprintln!("{warning}");
            }
        }
        Err(e) => {
            eprintln!("aurcache-sandbox container: refusing to run: {e}");
            return ExitCode::from(1);
        }
    }
    let err = std::process::Command::new(&request.program)
        .args(&request.args)
        .exec();
    eprintln!(
        "aurcache-sandbox container: cannot execute {}: {err}",
        request.program.to_string_lossy()
    );
    ExitCode::from(127)
}

fn parse_args(mut args: impl Iterator<Item = OsString>) -> Result<Request, String> {
    if args.next().is_none_or(|arg| arg != "--") {
        return Err("expected `--` after `container`".into());
    }
    let program = args.next().ok_or("missing PROGRAM after `--`")?;
    Ok(Request {
        program,
        args: args.collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> impl Iterator<Item = OsString> {
        list.iter()
            .map(OsString::from)
            .collect::<Vec<_>>()
            .into_iter()
    }

    #[test]
    fn a_program_follows_the_separator() {
        let request = parse_args(args(&["--", "systemd-nspawn", "-D", "/c", "--", "x"])).unwrap();
        assert_eq!(request.program, OsString::from("systemd-nspawn"));
        assert_eq!(request.args, vec!["-D", "/c", "--", "x"]);
    }

    #[test]
    fn the_separator_and_a_program_are_required() {
        assert!(parse_args(args(&["systemd-nspawn"])).is_err());
        assert!(parse_args(args(&["--"])).is_err());
        assert!(parse_args(args(&[])).is_err());
    }

    #[test]
    fn only_an_open_vsock_is_warned_of() {
        assert_eq!(seccomp::Vsock::Refused.warning(), None);
        let warning = seccomp::Vsock::Open("no seccomp filter".into())
            .warning()
            .unwrap();
        assert!(
            warning.contains("AF_VSOCK is not refused (no seccomp filter)"),
            "{warning}"
        );
    }
}
