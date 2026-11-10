/// Telling the logs about builds taken back from their workers.
pub mod abandoned;
/// Searching the AUR.
pub mod aur;
pub mod build_delete;
pub mod build_logger;
pub mod cancel;
/// Dependency version constraints: parsing, merging and comparing them.
pub mod constraint;
pub mod dump;
/// Git checkouts, and what a remote ref points at.
pub mod git;
pub mod job_config;
/// What a worker is handed when it claims a build.
pub mod job_descriptor;
pub mod listing;
pub mod package;
pub mod patch;
pub mod pkgbuild;
pub mod publish;
pub mod repository;
pub mod restore;
pub mod scheduled;
pub mod services;
pub mod settings;
pub mod snapshot;
/// The numbers the stats page and the dashboard show.
pub mod stats;
pub mod vcs_check;
pub mod worker_complete;
/// Registering workers, and approving them without an operator.
pub mod worker_enroll;
/// The fleet as an operator sees it.
pub mod worker_fleet;
pub mod worker_heartbeat;
/// How the server polices remote workers, read once from its environment.
pub mod worker_policy;
pub mod worker_settings;

#[cfg(test)]
#[path = "../test_support/git.rs"]
pub(crate) mod test_git;
