//! Put the agent where the build container can run it, and wait for the job.

use aurcache_build_agent::layout::{BIN_NAME, Work};
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::{Duration, Instant};

const POLL: Duration = Duration::from_millis(100);
/// The longest to wait, once the job is ready, for `receive` to exit.
const RECEIVED: Duration = Duration::from_secs(30);

/// Install this executable into `tools`, then return once the job is marked
/// ready and the `receive` that marked it has exited: `receive` is an exec
/// in this container, and the container ending would take it down before
/// the worker hears that it succeeded.
pub fn run(work: &Work, tools: &Path) -> io::Result<()> {
    install(tools)?;
    fs::create_dir_all(work.input())?;
    while fs::symlink_metadata(work.ready()).is_err() {
        std::thread::sleep(POLL);
    }
    let Some(pid) = fs::read_to_string(work.ready())
        .ok()
        .and_then(|text| text.trim().parse::<u32>().ok())
    else {
        return Ok(());
    };
    let process = format!("/proc/{pid}");
    let start = Instant::now();
    while Path::new(&process).exists() && start.elapsed() < RECEIVED {
        std::thread::sleep(POLL);
    }
    Ok(())
}

/// Copy this executable to `<tools>/aurcache-build-agent`, mode 0755, whole
/// before it has its name: the build container starts as soon as this exits.
fn install(tools: &Path) -> io::Result<()> {
    let part = tools.join(format!(".{BIN_NAME}.part"));
    fs::copy(std::env::current_exe()?, &part)
        .map_err(|error| io::Error::new(error.kind(), format!("installing the agent: {error}")))?;
    fs::set_permissions(&part, fs::Permissions::from_mode(0o755))?;
    fs::rename(&part, tools.join(BIN_NAME))
}

#[cfg(test)]
mod tests {
    use super::{install, run};
    use aurcache_build_agent::layout::Work;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;

    #[test]
    fn stage_returns_once_the_receive_that_marked_the_job_has_exited() {
        let dir = tempfile::tempdir().unwrap();
        let tools = tempfile::tempdir().unwrap();
        let work = Work::new(dir.path());
        let mut receive = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let staged = {
            let (work, tools) = (work.clone(), tools.path().to_path_buf());
            std::thread::spawn(move || run(&work, &tools))
        };
        std::thread::sleep(Duration::from_millis(300));
        assert!(!staged.is_finished(), "staged before the job was ready");
        std::fs::create_dir_all(work.input()).unwrap();
        let part = work.input().join(".ready.part");
        std::fs::write(&part, format!("{}\n", receive.id())).unwrap();
        std::fs::rename(&part, work.ready()).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        assert!(!staged.is_finished(), "staged while receive ran");
        receive.kill().unwrap();
        receive.wait().unwrap();
        staged.join().unwrap().unwrap();
    }

    #[test]
    fn install_leaves_an_executable_copy_and_no_part() {
        let tools = tempfile::tempdir().unwrap();
        install(tools.path()).unwrap();
        install(tools.path()).unwrap();
        let target = tools.path().join("aurcache-build-agent");
        assert_eq!(
            std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert_eq!(std::fs::read_dir(tools.path()).unwrap().count(), 1);
    }
}
