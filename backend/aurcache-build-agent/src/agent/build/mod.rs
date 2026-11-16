//! The build: the root step, the drop to the build user, the pacman helper,
//! makepkg, and the report.
//!
//! `container-workers.md` ("`build`, step by step") is what this does.

use std::path::Path;
use std::process::ExitCode;

/// Run the build in `work`, as PID 1 and root. Exits 0 if and only if it
/// wrote `<work>/out/report.json`.
pub fn run(work: &Path, log_file: Option<&Path>) -> ExitCode {
    // WP1 writes this.
    let _ = (work, log_file);
    unimplemented!()
}

/// `PACMAN_AUTH`'s client: send `argv` to root's helper and exit with
/// pacman's status, or 126 when the helper refused it.
pub fn pacman_request(argv: Vec<String>) -> ExitCode {
    // WP1 writes this.
    let _ = argv;
    unimplemented!()
}

/// Extract the source archive into `into`, as the build user.
pub fn unpack(archive: &Path, into: &Path) -> ExitCode {
    // WP1 writes this.
    let _ = (archive, into);
    unimplemented!()
}
