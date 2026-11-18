//! Put the agent where the build container can run it, and wait for the job.

use anyhow::{Context, Result};
use aurcache_build_agent::layout::{BIN_NAME, Work};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How often to look for the mark that the job is complete, and then for
/// the process that made it.
const POLL: Duration = Duration::from_millis(100);
/// The longest to wait, once the job is ready, for that process to exit.
const RECEIVED: Duration = Duration::from_secs(30);

/// Install this executable into `tools`, then return once `work`'s inputs
/// are marked ready and the `receive` that marked them has exited.
pub fn run(work: &Work, tools: &Path) -> Result<()> {
    install(tools)?;
    fs::create_dir_all(work.input())
        .with_context(|| format!("cannot create {}", work.input().display()))?;
    while fs::symlink_metadata(work.ready()).is_err() {
        std::thread::sleep(POLL);
    }
    wait_for_receiver(&work.ready());
    Ok(())
}

/// Wait for the process the ready mark names to have exited and been
/// reaped, which is when `/proc` no longer has it. `receive` runs as an exec
/// in this container, and this container ending while it still runs would
/// take it down. A mark that names no process is not waited on.
fn wait_for_receiver(ready: &Path) {
    let Some(pid) = fs::read_to_string(ready)
        .ok()
        .and_then(|text| text.trim().parse::<u32>().ok())
    else {
        return;
    };
    let process = PathBuf::from(format!("/proc/{pid}"));
    let start = Instant::now();
    while process.exists() && start.elapsed() < RECEIVED {
        std::thread::sleep(POLL);
    }
}

/// Copy this executable to `<tools>/aurcache-build-agent`, mode 0755. The
/// copy is complete before it has its final name, so a build container that
/// starts at once never runs half of it.
fn install(tools: &Path) -> Result<()> {
    let me = std::env::current_exe().context("cannot find this executable")?;
    fs::create_dir_all(tools).with_context(|| format!("cannot create {}", tools.display()))?;
    let part = tools.join(format!(".{BIN_NAME}.part"));
    let _ = fs::remove_file(&part);
    fs::copy(&me, &part).with_context(|| format!("cannot copy to {}", part.display()))?;
    fs::set_permissions(&part, fs::Permissions::from_mode(0o755))?;
    let target = tools.join(BIN_NAME);
    fs::rename(&part, &target).with_context(|| format!("cannot install {}", target.display()))
}

#[cfg(test)]
mod tests {
    use super::install;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn install_leaves_an_executable_copy_and_no_part() {
        let dir = tempfile::tempdir().unwrap();
        let tools = dir.path().join("tools");
        install(&tools).unwrap();
        install(&tools).unwrap();
        let target = tools.join("aurcache-build-agent");
        assert_eq!(
            std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert_eq!(std::fs::read_dir(&tools).unwrap().count(), 1);
    }
}
