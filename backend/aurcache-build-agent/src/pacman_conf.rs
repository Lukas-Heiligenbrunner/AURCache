//! What a job's `pacman.conf` may say.
//!
//! The agent writes the job's `pacman.conf` into `/etc` as root, so it refuses
//! one that sets what the agent decides itself (`CacheDir`, `DBPath`,
//! `RootDir`, `GPGDir`, `HookDir`, `LogFile`) or points a server at the
//! container's own files (`file://`).

/// Why a `pacman.conf` was refused: an option the agent sets itself, or a
/// `file://` server.
// The variants and their `Display` are WP1's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfRefusal {}

/// Accept `text` as a job's `pacman.conf`, or say why not.
pub fn check(text: &str) -> Result<(), ConfRefusal> {
    // WP1 writes this.
    let _ = text;
    unimplemented!()
}
