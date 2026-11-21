//! The worker's end of the SSH link, alone, for `scripts/test-build-agent.sh`:
//! starts an ssh-agent that has the key, runs the command after `--` (a
//! `docker exec -i ... ssh-link ...`), passes what comes over its stdin and
//! stdout to that ssh-agent until the link ends, and prints how it ended as
//! the build log would say it.
//!
//! ```text
//! ssh-link-serve <key> -- <command...>
//! ```

use aurcache_worker_container::ssh::agent::Agent;
use aurcache_worker_container::ssh::link::{self, LinkEnd};
use std::process::{ExitCode, Stdio};

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some((key, command)) = args.split_first() else {
        eprintln!("usage: ssh-link-serve <key> -- <command...>");
        return ExitCode::from(2);
    };
    let command = command.strip_prefix(&["--".to_string()]).unwrap_or(command);
    let Some((program, arguments)) = command.split_first() else {
        eprintln!("usage: ssh-link-serve <key> -- <command...>");
        return ExitCode::from(2);
    };
    let agent = match Agent::start(std::path::Path::new(key)).await {
        Ok(agent) => agent,
        Err(e) => {
            eprintln!("ssh-link-serve: {e:#}");
            return ExitCode::FAILURE;
        }
    };
    let mut child = match tokio::process::Command::new(program)
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            eprintln!("ssh-link-serve: {program}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        return ExitCode::FAILURE;
    };
    let served = link::serve(agent.socket(), stdout, stdin).await;
    let _ = child.wait().await;
    print!("{}", served.line());
    match served.end {
        LinkEnd::Closed => ExitCode::SUCCESS,
        LinkEnd::Lost { .. } | LinkEnd::Refused { .. } => ExitCode::FAILURE,
    }
}
